//! Row ordering and window-frame evaluation (mirrors impl/go window.go): the ORDER BY sorters
//! (sort_rows/sort_rows_collated and the decorated/collated row comparators), FrameCtx and its
//! ROWS/GROUPS/RANGE frame-bound arithmetic, apply_window_stage (the per-partition window driver), and
//! the value ordering primitives (key_cmp/value_cmp/family_rank).

use super::*;

/// Sort `rows` by the ORDER BY `order` keys (spec/design/grammar.md §10). The all-`C` fast path is a
/// stable `sort_by` over the value comparator; if ANY key carries a collation, the collation-aware
/// `sort_rows_collated` decorate-sorter runs instead (it can fail — an unmapped code point is 0A000).
pub(crate) fn sort_rows(rows: &mut Vec<Row>, order: &[crate::spill::SortKey]) -> Result<()> {
    if order.iter().any(|(_, _, _, c)| c.is_some()) {
        return sort_rows_collated(rows, order);
    }
    rows.sort_by(|a, b| cmp_rows_by_order(a, b, order));
    Ok(())
}

/// Materialize the general-expression ORDER BY keys before the sort (spec/design/grammar.md §10): for
/// each row evaluate every `order_exprs[k]` and append the value, so its sort slot `final_width + k`
/// reads the appended column and the slot-based comparator stays unchanged — the exact mechanism a
/// non-column window key uses (window.md §5.1, `apply_window_stage`). Runs over every pre-sort row
/// (before `LIMIT`, since the sort needs them all); the per-row evaluation is metered like a
/// projection (an `operator_eval` per node, charged inside `eval`). A no-op — and zero added cost —
/// when `order_exprs` is empty (a column/ordinal-only ORDER BY, byte-identical to before).
pub(crate) fn materialize_order_exprs(
    rows: &mut [Row],
    order_exprs: &[RExpr],
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<()> {
    if order_exprs.is_empty() {
        return Ok(());
    }
    for row in rows.iter_mut() {
        let mut vals = Vec::with_capacity(order_exprs.len());
        for oe in order_exprs {
            vals.push(oe.eval(row, env, meter)?);
        }
        row.extend(vals);
    }
    Ok(())
}

/// The WINDOW stage (spec/design/window.md §5.2): for each window function, partition the rows,
/// sort each partition by the window ORDER BY (stable → PK tie-break, as `rows` arrives in PK scan
/// order), compute the per-row result, and APPEND it to every row (so window result `i` lands at
/// flat slot `input_width + i`, where the projection reads it). The partition + sort are unmetered
/// (like ORDER BY / GROUP BY); each computed result charges `window_result` and guards the ceiling.
/// S0: `row_number()` only; partitions bucket value-canonically via an insertion-ordered list, so
/// no hash-map iteration order leaks (CLAUDE.md §8/§10).
///
/// The frame-sensitive plans (aggregate windows, first/last/nth_value) use a `FrameCtx`, which
/// precomputes the partition's peer-group structure once and maps each row to its `[lo, hi)` frame.
/// spec/design/window.md §6.

/// The integer count of a `ROWS`/`GROUPS` offset bound (widened to `i128` so the index arithmetic
/// cannot overflow). The offset is `Value::Int` by construction (resolve_int_bound).
pub(crate) fn offset_count(v: &Value) -> i128 {
    match v {
        Value::Int(k) => *k as i128,
        _ => unreachable!("ROWS/GROUPS offset is an integer count"),
    }
}

/// The sign of `v − (cur ∓ off)` for a `RANGE` value offset (window.md §6), computed in a wide
/// enough type to avoid overflow: integer keys compute the bound in `i128`; decimal keys use exact
/// decimal arithmetic; **float** keys widen to `f64` and compute the bound with the in-contract
/// correctly-rounded `+`/`-` kernel (float.md §5 — bit-identical cross-core), then compare with the
/// PG float total order (`total_cmp_f64`). The total order reproduces PG's `in_range` NaN handling
/// for free: a NaN current key makes the bound NaN (NaN ∓ finite = NaN), so a NaN row equals it and
/// any non-NaN row is below it, while a NaN row against a non-NaN bound sorts above — exactly PG's
/// "NaN sorts after non-NaN" rule. The offset is always finite (an int offset, or a decimal one that
/// would otherwise overflow already trapped at resolve), so `cur ∓ off` never produces NaN itself.
/// `subtract` chooses `cur − off` vs `cur + off`.
pub(crate) fn range_v_vs_bound(
    v: &Value,
    cur: &Value,
    off: &Value,
    subtract: bool,
) -> Result<std::cmp::Ordering> {
    match (cur, off) {
        (Value::Int(c), Value::Int(o)) => {
            let b = if subtract {
                *c as i128 - *o as i128
            } else {
                *c as i128 + *o as i128
            };
            let x = match v {
                Value::Int(x) => *x as i128,
                _ => unreachable!("RANGE integer-key value is Int"),
            };
            Ok(x.cmp(&b))
        }
        (Value::Decimal(c), Value::Decimal(o)) => {
            let b = if subtract { c.sub(o)? } else { c.add(o)? };
            Ok(value_cmp(v, &Value::Decimal(b)))
        }
        // Float key: widen to f64 (PG computes `in_range_float*_float8`'s sum in float8 even for an
        // f32 key) and compare in the float total order.
        (Value::Float32(_) | Value::Float64(_), Value::Float64(o)) => {
            let c = match cur {
                Value::Float32(c) => *c as f64,
                Value::Float64(c) => *c,
                _ => unreachable!(),
            };
            let x = match v {
                Value::Float32(x) => *x as f64,
                Value::Float64(x) => *x,
                _ => unreachable!("RANGE float-key value is a float"),
            };
            let b = if subtract { c - *o } else { c + *o };
            Ok(crate::value::total_cmp_f64(x, b))
        }
        _ => unreachable!("RANGE offset resolved to a matching numeric type"),
    }
}

/// One partition's peer-group structure (window.md §3/§6), shared across every row's frame lookup.
/// Peers are rows equal on the window ORDER BY keys; `peer_start`/`peer_end` bracket each row's peer
/// group, `group_of` is its peer-group ordinal, and `group_spans` lists every group's `[start, end)`.
struct FrameCtx<'a> {
    ordered: &'a [usize],
    rows: &'a [Row],
    order: &'a [crate::spill::SortKey],
    np: usize,
    peer_start: Vec<usize>,
    peer_end: Vec<usize>,
    group_of: Vec<usize>,
    group_spans: Vec<(usize, usize)>,
}

impl<'a> FrameCtx<'a> {
    pub(crate) fn new(
        ordered: &'a [usize],
        rows: &'a [Row],
        order: &'a [crate::spill::SortKey],
        coll_keys: &'a [Vec<Option<Vec<u8>>>],
    ) -> Self {
        let np = ordered.len();
        let mut group_spans: Vec<(usize, usize)> = Vec::new();
        let mut s = 0usize;
        for pos in 1..np {
            if cmp_window_rows(ordered[pos], ordered[s], rows, order, coll_keys)
                != std::cmp::Ordering::Equal
            {
                group_spans.push((s, pos));
                s = pos;
            }
        }
        if np > 0 {
            group_spans.push((s, np));
        }
        let mut peer_start = vec![0usize; np];
        let mut peer_end = vec![0usize; np];
        let mut group_of = vec![0usize; np];
        for (gi, &(a, b)) in group_spans.iter().enumerate() {
            for p in a..b {
                peer_start[p] = a;
                peer_end[p] = b;
                group_of[p] = gi;
            }
        }
        FrameCtx {
            ordered,
            rows,
            order,
            np,
            peer_start,
            peer_end,
            group_of,
            group_spans,
        }
    }

    /// The `[lo, hi)` frame for the row at sorted position `pos` (window.md §6). `None` ⇒ the
    /// default frame (RANGE UNBOUNDED PRECEDING TO CURRENT ROW = `[0, peer_end)`).
    pub(crate) fn bounds(
        &self,
        pos: usize,
        frame: &Option<ResolvedFrame>,
    ) -> Result<(usize, usize)> {
        use crate::ast::FrameMode;
        match frame {
            None => Ok((0, self.peer_end[pos])),
            Some(f) => match f.mode {
                FrameMode::Rows => Ok(self.rows_bounds(pos, f)),
                FrameMode::Groups => Ok(self.groups_bounds(pos, f)),
                FrameMode::Range => self.range_bounds(pos, f),
            },
        }
    }

    /// Whether sorted position `k` is dropped from the current row `pos`'s frame by `EXCLUDE`
    /// (window.md §6): `CURRENT ROW` drops the row itself, `GROUP` its whole peer group, `TIES` the
    /// peers but not the row, `NO OTHERS` nothing. Exclusion removes only rows already in `[lo, hi)`.
    pub(crate) fn is_excluded(
        &self,
        pos: usize,
        k: usize,
        exclude: crate::ast::FrameExclusion,
    ) -> bool {
        use crate::ast::FrameExclusion;
        match exclude {
            FrameExclusion::NoOthers => false,
            FrameExclusion::CurrentRow => k == pos,
            FrameExclusion::Group => self.peer_start[pos] <= k && k < self.peer_end[pos],
            FrameExclusion::Ties => k != pos && self.peer_start[pos] <= k && k < self.peer_end[pos],
        }
    }

    /// ROWS: physical row offsets in the partition sequence; bounds clamp to `[0, np]`.
    pub(crate) fn rows_bounds(&self, pos: usize, f: &ResolvedFrame) -> (usize, usize) {
        let p = pos as i128;
        let n = self.np as i128;
        let lo = match &f.start {
            ResolvedBound::UnboundedPreceding => 0,
            ResolvedBound::Preceding(k) => p - offset_count(k),
            ResolvedBound::CurrentRow => p,
            ResolvedBound::Following(k) => p + offset_count(k),
            ResolvedBound::UnboundedFollowing => n,
        };
        let hi = match &f.end {
            ResolvedBound::UnboundedPreceding => 0,
            ResolvedBound::Preceding(k) => p - offset_count(k) + 1,
            ResolvedBound::CurrentRow => p + 1,
            ResolvedBound::Following(k) => p + offset_count(k) + 1,
            ResolvedBound::UnboundedFollowing => n,
        };
        let lo = lo.clamp(0, n) as usize;
        let hi = hi.clamp(0, n) as usize;
        (lo, hi.max(lo))
    }

    /// GROUPS: peer-group offsets — a bound `g PRECEDING`/`FOLLOWING` lands on the `cg ∓ g`-th peer
    /// group's start (a start bound) or end (an end bound); a group index below 0 clamps to the
    /// partition start, at or above the group count to the partition end.
    pub(crate) fn groups_bounds(&self, pos: usize, f: &ResolvedFrame) -> (usize, usize) {
        let cg = self.group_of[pos] as i128;
        let g = self.group_spans.len() as i128;
        let np = self.np;
        let start_at = |j: i128| -> usize {
            if j < 0 {
                0
            } else if j >= g {
                np
            } else {
                self.group_spans[j as usize].0
            }
        };
        let end_at = |j: i128| -> usize {
            if j < 0 {
                0
            } else if j >= g {
                np
            } else {
                self.group_spans[j as usize].1
            }
        };
        let lo = match &f.start {
            ResolvedBound::UnboundedPreceding => 0,
            ResolvedBound::Preceding(k) => start_at(cg - offset_count(k)),
            ResolvedBound::CurrentRow => start_at(cg),
            ResolvedBound::Following(k) => start_at(cg + offset_count(k)),
            ResolvedBound::UnboundedFollowing => np,
        };
        let hi = match &f.end {
            ResolvedBound::UnboundedPreceding => 0,
            ResolvedBound::Preceding(k) => end_at(cg - offset_count(k)),
            ResolvedBound::CurrentRow => end_at(cg),
            ResolvedBound::Following(k) => end_at(cg + offset_count(k)),
            ResolvedBound::UnboundedFollowing => np,
        };
        (lo, hi.max(lo))
    }

    /// RANGE: logical offsets on the single ordering-key value (window.md §6). A bound with no
    /// offset (UNBOUNDED / CURRENT ROW) is peer/edge based and needs no key arithmetic. With a
    /// value offset, the frame spans the rows whose key is within the offset of the current key;
    /// a NULL current key has only its NULL peers (offset bounds collapse to the peer group, the
    /// PG rule), while UNBOUNDED bounds still reach the partition edge.
    pub(crate) fn range_bounds(&self, pos: usize, f: &ResolvedFrame) -> Result<(usize, usize)> {
        let np = self.np;
        let start_off = matches!(
            f.start,
            ResolvedBound::Preceding(_) | ResolvedBound::Following(_)
        );
        let end_off = matches!(
            f.end,
            ResolvedBound::Preceding(_) | ResolvedBound::Following(_)
        );
        if !start_off && !end_off {
            // Only UNBOUNDED / CURRENT ROW — peer/edge based (any number of ORDER BY keys).
            let lo = match &f.start {
                ResolvedBound::UnboundedPreceding => 0,
                _ => self.peer_start[pos], // CurrentRow
            };
            let hi = match &f.end {
                ResolvedBound::UnboundedFollowing => np,
                _ => self.peer_end[pos], // CurrentRow
            };
            return Ok((lo, hi.max(lo)));
        }
        // Offset present ⇒ exactly one ORDER BY key (validated at resolve).
        let (col, desc, _, _) = self.order[0];
        let cur = &self.rows[self.ordered[pos]][col];
        if matches!(cur, Value::Null) {
            let lo = match &f.start {
                ResolvedBound::UnboundedPreceding => 0,
                _ => self.peer_start[pos],
            };
            let hi = match &f.end {
                ResolvedBound::UnboundedFollowing => np,
                _ => self.peer_end[pos],
            };
            return Ok((lo, hi.max(lo)));
        }
        let lo = match &f.start {
            ResolvedBound::UnboundedPreceding => 0,
            ResolvedBound::CurrentRow => self.peer_start[pos],
            ResolvedBound::Preceding(off) => self.range_start(col, cur, off, true, desc)?,
            ResolvedBound::Following(off) => self.range_start(col, cur, off, false, desc)?,
            ResolvedBound::UnboundedFollowing => np,
        };
        let hi = match &f.end {
            ResolvedBound::UnboundedFollowing => np,
            ResolvedBound::CurrentRow => self.peer_end[pos],
            ResolvedBound::Preceding(off) => self.range_end(col, cur, off, true, desc, lo)?,
            ResolvedBound::Following(off) => self.range_end(col, cur, off, false, desc, lo)?,
            ResolvedBound::UnboundedPreceding => 0,
        };
        Ok((lo, hi.max(lo)))
    }

    /// The first sorted position whose key satisfies a RANGE start bound (NULL keys never qualify
    /// for a non-NULL current row). `subtract = is_preceding XOR descending` chooses the bound side.
    pub(crate) fn range_start(
        &self,
        col: usize,
        cur: &Value,
        off: &Value,
        is_preceding: bool,
        desc: bool,
    ) -> Result<usize> {
        let subtract = is_preceding != desc;
        for i in 0..self.np {
            let v = &self.rows[self.ordered[i]][col];
            if matches!(v, Value::Null) {
                continue;
            }
            let ord = range_v_vs_bound(v, cur, off, subtract)?;
            // ascending frame: v ≥ bound; descending frame: v ≤ bound.
            let include = if desc {
                ord != std::cmp::Ordering::Greater
            } else {
                ord != std::cmp::Ordering::Less
            };
            if include {
                return Ok(i);
            }
        }
        Ok(self.np)
    }

    /// The exclusive end of a RANGE end bound, scanning forward from `lo` while the key stays in
    /// frame (the in-frame keys form a contiguous run over the sorted partition).
    pub(crate) fn range_end(
        &self,
        col: usize,
        cur: &Value,
        off: &Value,
        is_preceding: bool,
        desc: bool,
        lo: usize,
    ) -> Result<usize> {
        let subtract = is_preceding != desc;
        let mut hi = lo;
        for i in lo..self.np {
            let v = &self.rows[self.ordered[i]][col];
            if matches!(v, Value::Null) {
                break;
            }
            let ord = range_v_vs_bound(v, cur, off, subtract)?;
            // ascending frame: v ≤ bound; descending frame: v ≥ bound.
            let include = if desc {
                ord != std::cmp::Ordering::Less
            } else {
                ord != std::cmp::Ordering::Greater
            };
            if include {
                hi = i + 1;
            } else {
                break;
            }
        }
        Ok(hi)
    }
}

/// Group window specs that share an identical PARTITION BY + ORDER BY (column slots + direction /
/// NULLS / collation), returning the spec indices per group. One partition + per-partition sort
/// then serves every spec in a group (window.md §5.2 — the shared partition/sort pass). Grouping is
/// stable and the per-spec slot mapping is preserved (each spec still writes its result column in
/// spec order), so the optimization is purely a wall-clock win — the cost is unchanged (§8).
pub(crate) fn group_window_specs(specs: &[WindowSpec]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    'spec: for (i, spec) in specs.iter().enumerate() {
        for g in groups.iter_mut() {
            let rep = &specs[g[0]];
            if rep.partition == spec.partition && rep.order == spec.order {
                g.push(i);
                continue 'spec;
            }
        }
        groups.push(vec![i]);
    }
    groups
}

/// Precompute each row's collated UCA sort-key bytes for `order`'s collated slots (window.md §3/§5),
/// indexed in parallel with `rows`, so the partition sort AND peer determination (ranking, frame
/// peer groups) honor the collation identically. Built once per group (the decorate pattern); an
/// unmapped code point fails 0A000 at this deterministic per-row point. Empty when no key is
/// collated, and the comparator stays on raw bytes.
pub(crate) fn window_coll_keys(
    rows: &[Row],
    order: &[crate::spill::SortKey],
) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
    if !order.iter().any(|(_, _, _, c)| c.is_some()) {
        return Ok(Vec::new());
    }
    let mut all = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let mut keys = Vec::new();
        for (idx, _, _, coll) in order {
            if let Some(c) = coll {
                keys.push(match &row[*idx] {
                    Value::Text(s) => Some(collation::sort_key(c, s)?),
                    _ => None, // NULL (a collated slot is text) — handled by NULL placement
                });
            }
        }
        all.push(keys);
    }
    Ok(all)
}

pub(crate) fn apply_window_stage(
    rows: &mut [Row],
    specs: &[WindowSpec],
    window_keys: &[RExpr],
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<()> {
    let n = rows.len();
    if n == 0 {
        return Ok(());
    }
    // Materialize the non-column PARTITION BY / ORDER BY key expressions (window.md §5.1): evaluate
    // each against the row and append it, so a materialized key's slot `input_width + k` reads the
    // appended column and the partition / sort / frame machinery below (all slot-based) is unchanged.
    // The window results are appended AFTER these, so a result slot is `input_width + window_keys.len()
    // + w` (the rebased projection slot). Empty for a column-only window — no appended columns, the
    // result slot stays `input_width + w`, byte-identical to before. The key evaluation is metered
    // like any expression (operator_eval per node): new, deterministic, cross-core-identical work that
    // exists only for an expression key (a bare-column key is not in `window_keys`).
    if !window_keys.is_empty() {
        for row in rows.iter_mut() {
            let mut kv = Vec::with_capacity(window_keys.len());
            for ke in window_keys {
                kv.push(ke.eval(row, env, meter)?);
            }
            row.extend(kv);
        }
    }
    // The shared partition/sort pass (window.md §5.2): specs that share an identical PARTITION BY +
    // ORDER BY are partitioned and sorted ONCE (the expensive step), then each computes its own
    // results over the shared sorted partitions. The partition + sort are unmetered (§8), so this is
    // purely a wall-clock win — the per-spec result/frame metering, and thus the cost, are unchanged.
    let groups = group_window_specs(specs);
    let mut spec_group = vec![0usize; specs.len()];
    let mut group_cache: Vec<(Vec<Vec<usize>>, Vec<Vec<Option<Vec<u8>>>>)> =
        Vec::with_capacity(groups.len());
    for (gi, group) in groups.iter().enumerate() {
        let rep = &specs[group[0]];
        for &si in group {
            spec_group[si] = gi;
        }
        // Partition the row indices by the partition-key values. The map is an index only (never
        // iterated); output comes from the insertion-ordered `partitions` (no hash-order leak).
        let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
        let mut partitions: Vec<Vec<usize>> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let key: Vec<Value> = rep.partition.iter().map(|&p| row[p].clone()).collect();
            let pi = match index.get(&key) {
                Some(&p) => p,
                None => {
                    let p = partitions.len();
                    index.insert(key, p);
                    partitions.push(Vec::new());
                    p
                }
            };
            partitions[pi].push(i);
        }
        // Collated UCA sort-key bytes for the shared order's collated slots (window.md §3/§5), built
        // once per group; an unmapped code point fails 0A000 here. Empty when no key is collated.
        let coll_keys = window_coll_keys(rows, &rep.order)?;
        // Sort each partition by the shared window ORDER BY. `slice::sort_by` is stable, so a full
        // tie keeps ascending original index = PK scan order (the §3 PK tie-break).
        for part in &mut partitions {
            if !rep.order.is_empty() {
                part.sort_by(|&a, &b| cmp_window_rows(a, b, rows, &rep.order, &coll_keys));
            }
        }
        group_cache.push((partitions, coll_keys));
    }
    for (si, spec) in specs.iter().enumerate() {
        // Reuse this spec's group's shared sorted partitions + collation keys (computed once above).
        // `ordered` is cloned per partition (a cheap index vector; the costly sort is shared) and
        // `coll_keys` per spec, so the per-plan computation below reads them by value, unchanged.
        let (sorted_partitions, coll_keys) = &group_cache[spec_group[si]];
        let coll_keys = coll_keys.clone();
        let mut results: Vec<Value> = vec![Value::Null; n];
        for ordered in sorted_partitions.iter().cloned() {
            match spec.plan {
                WindowPlan::RowNumber => {
                    for (pos, &ri) in ordered.iter().enumerate() {
                        meter.guard()?; // enforce the cost ceiling per result (CLAUDE.md §13)
                        meter.charge(COSTS.window_result);
                        results[ri] = Value::Int(pos as i64 + 1);
                    }
                }
                // Peer-aware ranking (window.md §3/§4): peers are rows EQUAL on the window ORDER BY
                // keys only (the PK tie-break sequences peers but does not split a peer group). A
                // single pass identifies peer-group spans [start, end) over the sorted partition; an
                // empty ORDER BY makes the whole partition one peer group. From each row's span:
                // rank = start+1, dense_rank = group ordinal, percent_rank = start/(N-1) (0 if N=1),
                // cume_dist = end/N. The ratios are f64 (PG's float8, window.md §4): one IEEE
                // correctly-rounded division of small integers that convert exactly to binary64, so
                // the value is bit-identical across cores and to PG (the in-contract kernel, float.md §5).
                WindowPlan::Rank
                | WindowPlan::DenseRank
                | WindowPlan::PercentRank
                | WindowPlan::CumeDist => {
                    let np = ordered.len();
                    let mut groups: Vec<(usize, usize)> = Vec::new(); // peer-group spans [start, end)
                    let mut s = 0usize;
                    for pos in 1..np {
                        if cmp_window_rows(ordered[pos], ordered[s], rows, &spec.order, &coll_keys)
                            != std::cmp::Ordering::Equal
                        {
                            groups.push((s, pos));
                            s = pos;
                        }
                    }
                    if np > 0 {
                        groups.push((s, np));
                    }
                    for (gi, &(start, end)) in groups.iter().enumerate() {
                        for &ri in &ordered[start..end] {
                            meter.guard()?;
                            meter.charge(COSTS.window_result);
                            results[ri] = match spec.plan {
                                WindowPlan::Rank => Value::Int(start as i64 + 1),
                                WindowPlan::DenseRank => Value::Int(gi as i64 + 1),
                                WindowPlan::PercentRank => {
                                    if np <= 1 {
                                        Value::Float64(0.0)
                                    } else {
                                        Value::Float64(start as f64 / (np - 1) as f64)
                                    }
                                }
                                // cume_dist: rows through the current peer group / N.
                                _ => Value::Float64(end as f64 / np as f64),
                            };
                        }
                    }
                }
                // ntile(n): distribute the partition into n ranked buckets, larger buckets first
                // (window.md §4). n is evaluated once (the first sorted row); NULL n → NULL for all;
                // n ≤ 0 → 22014. Position-based: bucket boundaries are by sorted position, not peers.
                WindowPlan::Ntile => {
                    let np = ordered.len();
                    match spec.args[0].eval(&rows[ordered[0]], env, meter)? {
                        // NULL bucket count → NULL for every row (PG).
                        Value::Null => {
                            for &ri in &ordered {
                                meter.guard()?;
                                meter.charge(COSTS.window_result);
                                results[ri] = Value::Null;
                            }
                        }
                        Value::Int(nbuckets) => {
                            if nbuckets <= 0 {
                                return Err(EngineError::new(
                                    SqlState::InvalidArgumentForNtile,
                                    "argument of ntile must be greater than zero",
                                ));
                            }
                            let nbuckets = nbuckets as usize;
                            let base = np / nbuckets; // floor rows per bucket
                            let rem = np % nbuckets; // the first `rem` buckets get one extra row
                            let big = rem * (base + 1); // rows in the larger (base+1) buckets
                            for (pos, &ri) in ordered.iter().enumerate() {
                                meter.guard()?;
                                meter.charge(COSTS.window_result);
                                // Larger buckets first: positions [0, big) → (base+1)-sized buckets,
                                // the rest → base-sized buckets. `base` is 0 only when nbuckets > np,
                                // and then every pos < big so the else branch never divides by 0.
                                let bucket = if pos < big {
                                    pos / (base + 1) + 1
                                } else {
                                    rem + (pos - big) / base + 1
                                };
                                results[ri] = Value::Int(bucket as i64);
                            }
                        }
                        _ => unreachable!("ntile argument resolved to integer"),
                    }
                }
                // lag/lead (window.md §4): the value `offset` positions back (lag) / forward (lead)
                // in the partition, else the default (or NULL). Frame-insensitive — offset is by
                // sorted position. The value is evaluated for every row; offset once (NULL → all
                // NULL); the default per out-of-range row.
                WindowPlan::Lag | WindowPlan::Lead => {
                    let np = ordered.len();
                    let mut vals: Vec<Value> = Vec::with_capacity(np);
                    for &ri in &ordered {
                        vals.push(spec.args[0].eval(&rows[ri], env, meter)?);
                    }
                    let offset = if spec.args.len() >= 2 {
                        match spec.args[1].eval(&rows[ordered[0]], env, meter)? {
                            Value::Null => None, // NULL offset → NULL for every row (PG)
                            Value::Int(o) => Some(o),
                            _ => unreachable!("lag/lead offset resolved to integer"),
                        }
                    } else {
                        Some(1)
                    };
                    let dir: i64 = if matches!(spec.plan, WindowPlan::Lead) {
                        1
                    } else {
                        -1
                    };
                    for (pos, &ri) in ordered.iter().enumerate() {
                        meter.guard()?;
                        meter.charge(COSTS.window_result);
                        results[ri] = match offset {
                            None => Value::Null,
                            Some(o) => {
                                let target = pos as i64 + dir * o;
                                if target >= 0 && (target as usize) < np {
                                    vals[target as usize].clone()
                                } else if spec.args.len() == 3 {
                                    spec.args[2].eval(&rows[ri], env, meter)?
                                } else {
                                    Value::Null
                                }
                            }
                        };
                    }
                }
                // An aggregate over the default frame (window.md §6): RANGE UNBOUNDED PRECEDING TO
                // CURRENT ROW with a window ORDER BY (a RUNNING aggregate — CURRENT ROW spans the
                // current peer group), or the WHOLE partition with no ORDER BY. Both reduce to the
                // same shape: fold rows in sorted order, snapshotting the running `Acc` at each
                // peer-group boundary (no ORDER BY → one peer group → one whole-partition value).
                WindowPlan::Agg(aggplan) => {
                    let np = ordered.len();
                    let has_operand = !spec.args.is_empty(); // COUNT(*) has no operand
                    let opval = |k: usize, m: &mut Meter| -> Result<Value> {
                        if has_operand {
                            spec.args[0].eval(&rows[ordered[k]], env, m)
                        } else {
                            Ok(Value::Null)
                        }
                    };
                    // FILTER (WHERE cond): a frame row whose filter is not TRUE does not fold into the
                    // window aggregate (aggregates.md §20). Evaluated per visited frame row (charging
                    // its operator_evals); `None` keeps every row. A FILTER forces the naive re-fold
                    // path for explicit frames (a filtered row cannot be cleanly un-folded).
                    let filter_pass = |k: usize, m: &mut Meter| -> Result<bool> {
                        match &spec.filter {
                            Some(f) => Ok(f.eval(&rows[ordered[k]], env, m)?.is_true()),
                            None => Ok(true),
                        }
                    };
                    if spec.frame.is_none() {
                        // DEFAULT frame: a single running pass, snapshotting the accumulator at each
                        // peer-group boundary (window.md §6) — O(n).
                        let mut groups: Vec<(usize, usize)> = Vec::new();
                        let mut s = 0usize;
                        for pos in 1..np {
                            if cmp_window_rows(
                                ordered[pos],
                                ordered[s],
                                rows,
                                &spec.order,
                                &coll_keys,
                            ) != std::cmp::Ordering::Equal
                            {
                                groups.push((s, pos));
                                s = pos;
                            }
                        }
                        if np > 0 {
                            groups.push((s, np));
                        }
                        let mut acc = Acc::new(aggplan);
                        for &(start, end) in &groups {
                            for k in start..end {
                                meter.charge(COSTS.window_frame_step);
                                if !filter_pass(k, meter)? {
                                    continue; // FILTER excludes this row from the running fold
                                }
                                let v = opval(k, meter)?;
                                acc.fold(v, meter)?;
                            }
                            let out = acc.clone().finalize()?;
                            for &ri in &ordered[start..end] {
                                meter.guard()?;
                                meter.charge(COSTS.window_result);
                                results[ri] = out.clone();
                            }
                        }
                    } else {
                        // EXPLICIT frame (window.md §5.2/§6). The sorted partition makes the frame
                        // bounds [lo, hi) monotonic non-decreasing in `pos`, so a NO-EXCLUDE
                        // aggregate CARRIES one accumulator across rows rather than re-folding each
                        // frame from scratch (the sliding-window optimization):
                        //   • an EXPANDING frame (start UNBOUNDED PRECEDING ⇒ lo ≡ 0) folds each
                        //     entering row once as `hi` advances — byte-identical for EVERY
                        //     aggregate, since the fold order is the sorted-prefix order the naive
                        //     path uses (overflow traps / canonical float fold / decimal scale all
                        //     match) — O(n);
                        //   • a MOVING frame additionally UN-folds the rows leaving on the left, but
                        //     only for the exactly-invertible COUNT / COUNT(*) — O(n);
                        //   • a MOVING frame over SUM/AVG/MIN/MAX/float (not safely invertible) and
                        //     ANY frame with EXCLUDE re-fold from scratch (the naive O(partition²)).
                        // `window_frame_step` is charged per folded AND per un-folded row, so it only
                        // LOWERS; each row's operand is evaluated at most once (cached in `vals`), so
                        // `operator_eval` never rises.
                        let ctx = FrameCtx::new(&ordered, rows, &spec.order, &coll_keys);
                        let exclude = spec
                            .frame
                            .as_ref()
                            .map_or(crate::ast::FrameExclusion::NoOthers, |f| f.exclude);
                        let mut vals: Vec<Option<Value>> = vec![None; np];
                        let eval_at = |k: usize,
                                       m: &mut Meter,
                                       vals: &mut Vec<Option<Value>>|
                         -> Result<Value> {
                            if !has_operand {
                                return Ok(Value::Null);
                            }
                            if vals[k].is_none() {
                                vals[k] = Some(spec.args[0].eval(&rows[ordered[k]], env, m)?);
                            }
                            Ok(vals[k].as_ref().unwrap().clone())
                        };
                        if exclude != crate::ast::FrameExclusion::NoOthers || spec.filter.is_some()
                        {
                            // EXCLUDE or FILTER breaks the clean add/remove model → naive per-row
                            // re-fold (the dropped rows are neither metered nor counted), over the
                            // cached operand. A FILTER additionally skips a non-TRUE frame row.
                            for pos in 0..np {
                                let (lo, hi) = ctx.bounds(pos, &spec.frame)?;
                                let mut acc = Acc::new(aggplan);
                                for k in lo..hi {
                                    if ctx.is_excluded(pos, k, exclude) {
                                        continue;
                                    }
                                    meter.charge(COSTS.window_frame_step);
                                    if !filter_pass(k, meter)? {
                                        continue;
                                    }
                                    let v = eval_at(k, meter, &mut vals)?;
                                    acc.fold(v, meter)?;
                                }
                                meter.guard()?;
                                meter.charge(COSTS.window_result);
                                results[ordered[pos]] = acc.finalize()?;
                            }
                        } else {
                            // SLIDING (monotone carry). `removable` aggregates un-fold the left edge;
                            // the rest rebuild when `lo` advances (an expanding frame never advances
                            // `lo`, so it only ever adds — the universal byte-identical case).
                            let removable = matches!(aggplan, AggPlan::CountStar | AggPlan::Count);
                            let mut acc = Acc::new(aggplan);
                            let mut cur_lo = 0usize;
                            let mut cur_hi = 0usize;
                            for pos in 0..np {
                                let (lo, hi) = ctx.bounds(pos, &spec.frame)?;
                                if !removable && lo > cur_lo {
                                    // Left edge advanced over a non-invertible aggregate ⇒ rebuild.
                                    acc = Acc::new(aggplan);
                                    for k in lo..hi {
                                        meter.charge(COSTS.window_frame_step);
                                        let v = eval_at(k, meter, &mut vals)?;
                                        acc.fold(v, meter)?;
                                    }
                                } else {
                                    // Un-fold rows leaving on the left (invertible only; empty when
                                    // `lo == cur_lo`) …
                                    let rem_hi = lo.min(cur_hi);
                                    for k in cur_lo..rem_hi {
                                        meter.charge(COSTS.window_frame_step);
                                        let v = eval_at(k, meter, &mut vals)?;
                                        acc.unfold(v, meter)?;
                                    }
                                    // … and fold rows entering on the right.
                                    let add_lo = cur_hi.max(lo);
                                    for k in add_lo..hi {
                                        meter.charge(COSTS.window_frame_step);
                                        let v = eval_at(k, meter, &mut vals)?;
                                        acc.fold(v, meter)?;
                                    }
                                }
                                cur_lo = lo;
                                cur_hi = hi;
                                meter.guard()?;
                                meter.charge(COSTS.window_result);
                                results[ordered[pos]] = acc.clone().finalize()?;
                            }
                        }
                    }
                }
                // Frame-sensitive value pickers (S4, window.md §4): first/last/nth row of the frame.
                WindowPlan::FirstValue | WindowPlan::LastValue | WindowPlan::NthValue => {
                    let np = ordered.len();
                    // The value expression, evaluated once per row (sorted order).
                    let mut vals: Vec<Value> = Vec::with_capacity(np);
                    for &ri in &ordered {
                        vals.push(spec.args[0].eval(&rows[ri], env, meter)?);
                    }
                    // nth_value's position — evaluated once; NULL → NULL for all; < 1 → 22016.
                    let nth = if matches!(spec.plan, WindowPlan::NthValue) {
                        match spec.args[1].eval(&rows[ordered[0]], env, meter)? {
                            Value::Null => None,
                            Value::Int(n) if n >= 1 => Some(n as usize),
                            Value::Int(_) => {
                                return Err(EngineError::new(
                                    SqlState::InvalidArgumentForNthValue,
                                    "argument of nth_value must be greater than zero",
                                ));
                            }
                            _ => unreachable!("nth_value position resolved to integer"),
                        }
                    } else {
                        Some(0) // unused for first/last
                    };
                    let ctx = FrameCtx::new(&ordered, rows, &spec.order, &coll_keys);
                    let exclude = spec
                        .frame
                        .as_ref()
                        .map_or(crate::ast::FrameExclusion::NoOthers, |f| f.exclude);
                    for pos in 0..np {
                        meter.guard()?;
                        meter.charge(COSTS.window_result);
                        let (lo, hi) = ctx.bounds(pos, &spec.frame)?;
                        // first/last/nth pick over the frame's NON-excluded rows (window.md §6); the
                        // `NoOthers` fast path breaks on the first row, so it stays O(1).
                        results[ordered[pos]] = match spec.plan {
                            WindowPlan::FirstValue => (lo..hi)
                                .find(|&k| !ctx.is_excluded(pos, k, exclude))
                                .map_or(Value::Null, |k| vals[k].clone()),
                            WindowPlan::LastValue => (lo..hi)
                                .rev()
                                .find(|&k| !ctx.is_excluded(pos, k, exclude))
                                .map_or(Value::Null, |k| vals[k].clone()),
                            WindowPlan::NthValue => match nth {
                                Some(n) => (lo..hi)
                                    .filter(|&k| !ctx.is_excluded(pos, k, exclude))
                                    .nth(n - 1)
                                    .map_or(Value::Null, |k| vals[k].clone()),
                                None => Value::Null, // NULL n
                            },
                            _ => Value::Null,
                        };
                    }
                }
            }
        }
        for (i, row) in rows.iter_mut().enumerate() {
            row.push(std::mem::replace(&mut results[i], Value::Null));
        }
    }
    Ok(())
}

/// Compare two rows by the (all-`C`) ORDER BY keys — the first non-equal key decides; a full tie is
/// Equal (the stable sort then keeps input order). Only used when no key is collated.
pub(crate) fn cmp_rows_by_order(
    a: &Row,
    b: &Row,
    order: &[crate::spill::SortKey],
) -> std::cmp::Ordering {
    for (idx, descending, nulls_first, _coll) in order {
        let ord = key_cmp(&a[*idx], &b[*idx], *descending, *nulls_first);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Compare two rows of the window buffer (by their index `a`/`b` into the full row slice) by the
/// window ORDER BY keys, honoring collation. A collated slot compares the precomputed UCA sort-key
/// bytes in `coll_keys` (indexed in parallel with the rows; NULL placement + the descending flip
/// applied here, mirroring `cmp_decorated`); a non-collated slot compares the row values via
/// `key_cmp`. This one comparator drives the partition sort AND every peer determination (ranking,
/// the aggregate default frame, `FrameCtx`'s peer groups), so a collated window orders, ranks, and
/// frames identically (window.md §3/§5). With no collated key, `coll_keys` is unused and this is
/// `cmp_rows_by_order` by index.
pub(crate) fn cmp_window_rows(
    a: usize,
    b: usize,
    rows: &[Row],
    order: &[crate::spill::SortKey],
    coll_keys: &[Vec<Option<Vec<u8>>>],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ci = 0usize; // advances once per collated slot (keys stored in slot order)
    for (idx, descending, nulls_first, coll) in order {
        let ord = if coll.is_some() {
            let ak = &coll_keys[a][ci];
            let bk = &coll_keys[b][ci];
            ci += 1;
            match (ak, bk) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => {
                    if *nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), None) => {
                    if *nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (Some(x), Some(y)) => {
                    let base = x.cmp(y);
                    if *descending { base.reverse() } else { base }
                }
            }
        } else {
            key_cmp(&rows[a][*idx], &rows[b][*idx], *descending, *nulls_first)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Sort `rows` when at least one ORDER BY key is collated (spec/design/collation.md §6/§8).
/// Decorate-sort-undecorate: each collated key's UCA sort key is built ONCE per row up front
/// (propagating a `sort_key` failure — e.g. 0A000 for an unmapped code point — at this deterministic
/// per-row point, not inside the comparator), then the rows are sorted by the precomputed key bytes
/// for collated slots and the value comparator for the rest. The sort is UNMETERED like every sort
/// (cost.md §3); the `collate` cost is charged at the comparison evaluator (collation.md §11). A
/// collated ORDER BY is in-memory only this slice, so this never spills (collated keys are slice 1e).
pub(crate) fn sort_rows_collated(
    rows: &mut Vec<Row>,
    order: &[crate::spill::SortKey],
) -> Result<()> {
    let mut decorated: Vec<(Vec<Option<Vec<u8>>>, Row)> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let keys = collation_keys_for_row(&row, order)?;
        decorated.push((keys, row));
    }
    decorated.sort_by(|a, b| cmp_decorated(a, b, order));
    rows.extend(decorated.into_iter().map(|(_, row)| row));
    Ok(())
}

/// Compare two decorated rows (precomputed collated-key bytes + the row) by the ORDER BY keys. A
/// collated slot compares its precomputed sort-key bytes (NULL placement + the descending flip
/// applied here, mirroring `key_cmp`); a non-collated slot compares the row values via `key_cmp`.
pub(crate) fn cmp_decorated(
    a: &(Vec<Option<Vec<u8>>>, Row),
    b: &(Vec<Option<Vec<u8>>>, Row),
    order: &[crate::spill::SortKey],
) -> std::cmp::Ordering {
    cmp_decorated_parts(&a.0, &a.1, &b.0, &b.1, order)
}

fn cmp_decorated_parts(
    akeys: &[Option<Vec<u8>>],
    arow: &Row,
    bkeys: &[Option<Vec<u8>>],
    brow: &Row,
    order: &[crate::spill::SortKey],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ci = 0usize; // advances once per collated slot (keys stored in slot order)
    for (idx, descending, nulls_first, coll) in order {
        let ord = if coll.is_some() {
            let ak = &akeys[ci];
            let bk = &bkeys[ci];
            ci += 1;
            match (ak, bk) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => {
                    if *nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), None) => {
                    if *nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (Some(x), Some(y)) => {
                    let base = x.cmp(y);
                    if *descending { base.reverse() } else { base }
                }
            }
        } else {
            key_cmp(&arow[*idx], &brow[*idx], *descending, *nulls_first)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn collation_keys_for_row(
    row: &Row,
    order: &[crate::spill::SortKey],
) -> Result<Vec<Option<Vec<u8>>>> {
    let mut keys = Vec::new();
    for (idx, _, _, coll) in order {
        if let Some(c) = coll {
            keys.push(match &row[*idx] {
                Value::Text(s) => Some(collation::sort_key(c, s)?),
                _ => None,
            });
        }
    }
    Ok(keys)
}

struct TopKItem {
    keys: Vec<Option<Vec<u8>>>,
    row: Row,
    pos: usize,
}

/// A bounded max-heap whose root is the worst retained row. Input position completes the exact
/// ORDER BY comparator, preserving stable full-sort ties even though heap layout is unstable.
pub(crate) struct TopKKeeper {
    k: usize,
    next_pos: usize,
    collated: bool,
    order: Vec<crate::spill::SortKey>,
    items: Vec<TopKItem>,
}

impl TopKKeeper {
    pub(crate) fn new(k: i64, order: &[crate::spill::SortKey], collated: bool) -> Self {
        Self {
            k: usize::try_from(k).unwrap_or(usize::MAX),
            next_pos: 0,
            collated,
            order: order.to_vec(),
            items: Vec::new(),
        }
    }

    fn cmp(&self, a: &TopKItem, b: &TopKItem) -> std::cmp::Ordering {
        let by_key = if self.collated {
            cmp_decorated_parts(&a.keys, &a.row, &b.keys, &b.row, &self.order)
        } else {
            cmp_rows_by_order(&a.row, &b.row, &self.order)
        };
        by_key.then_with(|| a.pos.cmp(&b.pos))
    }

    pub(crate) fn push(&mut self, row: Row) -> Result<()> {
        let keys = if self.collated {
            collation_keys_for_row(&row, &self.order)?
        } else {
            Vec::new()
        };
        let item = TopKItem {
            keys,
            row,
            pos: self.next_pos,
        };
        self.next_pos += 1;
        // Collated LIMIT 0 still decorates every row so its deterministic failure point is unchanged.
        if self.k == 0 {
            return Ok(());
        }
        if self.items.len() < self.k {
            self.items.push(item);
            self.sift_up(self.items.len() - 1);
        } else if self.cmp(&item, &self.items[0]).is_lt() {
            self.items[0] = item;
            self.sift_down(0);
        }
        Ok(())
    }

    fn sift_up(&mut self, mut child: usize) {
        while child > 0 {
            let parent = (child - 1) / 2;
            if !self.cmp(&self.items[child], &self.items[parent]).is_gt() {
                break;
            }
            self.items.swap(child, parent);
            child = parent;
        }
    }

    fn sift_down(&mut self, mut parent: usize) {
        loop {
            let left = parent * 2 + 1;
            if left >= self.items.len() {
                break;
            }
            let right = left + 1;
            let mut worst = left;
            if right < self.items.len() && self.cmp(&self.items[right], &self.items[left]).is_gt() {
                worst = right;
            }
            if !self.cmp(&self.items[worst], &self.items[parent]).is_gt() {
                break;
            }
            self.items.swap(parent, worst);
            parent = worst;
        }
    }

    pub(crate) fn finish(mut self) -> Vec<Row> {
        let collated = self.collated;
        let order = self.order;
        self.items.sort_by(|a, b| {
            let by_key = if collated {
                cmp_decorated_parts(&a.keys, &a.row, &b.keys, &b.row, &order)
            } else {
                cmp_rows_by_order(&a.row, &b.row, &order)
            };
            by_key.then_with(|| a.pos.cmp(&b.pos))
        });
        self.items.into_iter().map(|item| item.row).collect()
    }
}

pub(crate) fn top_k_rows(
    rows: Vec<Row>,
    order: &[crate::spill::SortKey],
    k: i64,
) -> Result<Vec<Row>> {
    let collated = order.iter().any(|key| key.3.is_some());
    let mut keeper = TopKKeeper::new(k, order, collated);
    for row in rows {
        keeper.push(row)?;
    }
    Ok(keeper.finish())
}

/// One ORDER BY key's total-order comparison. NULL placement is governed by `nulls_first`
/// and applied INDEPENDENTLY of the value-direction flip (`descending`), so an explicit
/// `NULLS FIRST|LAST` overrides the direction default (spec/design/grammar.md §10). The
/// physical key order ratifies NULL as the largest value (the PostgreSQL model), which
/// surfaces as the parse-time default `nulls_first = descending` (ASC → last, DESC → first).
pub(crate) fn key_cmp(
    a: &Value,
    b: &Value,
    descending: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        _ => {
            let base = value_cmp(a, b);
            if descending { base.reverse() } else { base }
        }
    }
}

/// Total order over NON-NULL values: signed-integer ascending, text by the `C`
/// collation — raw UTF-8 bytes, which for UTF-8 equals code-point order
/// (spec/design/types.md §11) — and boolean by value, false < true (types.md §9). The
/// cross-family arms (a fixed `bool < int < text` order) are kept only for totality —
/// ORDER BY is over a single typed column, so they are unreachable from SELECT. NULLs are
/// handled by `key_cmp` before this is reached.
pub(crate) fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => x.cmp_value(y),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        // Floats order by the PG total order (NaN largest, -0 = +0; spec/design/float.md §3).
        (Value::Float32(x), Value::Float32(y)) => crate::value::total_cmp_f32(*x, *y),
        (Value::Float64(x), Value::Float64(y)) => crate::value::total_cmp_f64(*x, *y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        // Timestamps order by the i64 instant (-infinity < finite < infinity).
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        // Intervals order by the canonical 128-bit span (spec/design/interval.md §2).
        (Value::Interval(x), Value::Interval(y)) => x.cmp(y),
        // A composite sorts lexicographically, NULLs-last per field (the composite sort key —
        // spec/design/composite.md §5): the first non-equal field decides, recursing through
        // `key_cmp` so per-field NULL placement and nested composites are handled uniformly. The
        // caller's `descending` flip in `key_cmp` reverses the whole tuple. A row-size tie-break
        // keeps it total (same-type rows have equal arity, so it is only reached for safety).
        (Value::Composite(x), Value::Composite(y)) => {
            for (xf, yf) in x.iter().zip(y.iter()) {
                let c = key_cmp(xf, yf, false, false);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        // An array sorts by the PG `array_cmp` total order (spec/design/array.md §5): element-wise
        // over the flattened elements (NULLs-last per element, recursing through `key_cmp`), then
        // fewer elements first, then smaller ndim, then per dimension (length, then lower bound).
        (Value::Array(x), Value::Array(y)) => {
            for (xe, ye) in x.elements.iter().zip(y.elements.iter()) {
                let c = key_cmp(xe, ye, false, false);
                if c != Ordering::Equal {
                    return c;
                }
            }
            let mut c = x.elements.len().cmp(&y.elements.len());
            if c != Ordering::Equal {
                return c;
            }
            c = x.dims.len().cmp(&y.dims.len());
            if c != Ordering::Equal {
                return c;
            }
            for d in 0..x.dims.len() {
                c = x.dims[d]
                    .cmp(&y.dims[d])
                    .then(x.lbounds[d].cmp(&y.lbounds[d]));
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
        // A range sorts by the PG `range_cmp` total order (spec/design/ranges.md §6): `empty` below
        // every non-empty, then lower bound, then upper bound (accounting for infinity/inclusivity).
        // Kept identical to `value::lt3`/`gt3`'s range arm so `<` and `ORDER BY` never disagree.
        (Value::Range(x), Value::Range(y)) => crate::range::range_total_cmp(x, y),
        // jsonb sorts by PG's total btree order (spec/design/json.md §5); kept identical to
        // `value::lt3`/`gt3`'s jsonb arm so `<` and `ORDER BY` never disagree. (json never sorts —
        // the resolver rejects it 42883.)
        (Value::Jsonb(x), Value::Jsonb(y)) => x.cmp(y),
        (Value::Null, Value::Null) => Ordering::Equal,
        // Cross-family arms exist only for totality — ORDER BY is over a single typed column,
        // so a mixed pair is unreachable. A fixed family order keeps the comparator total.
        _ => family_rank(a).cmp(&family_rank(b)),
    }
}

/// A fixed total order across value families, used only to keep `value_cmp` total for the
/// unreachable cross-family case (ORDER BY is single-column-typed).
pub(crate) fn family_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Decimal(_) => 3,
        Value::Text(_) => 4,
        Value::Bytea(_) => 5,
        Value::Uuid(_) => 6,
        Value::Timestamp(_) => 6,
        Value::Timestamptz(_) => 7,
        Value::Interval(_) => 8,
        Value::Float32(_) => 9,
        Value::Float64(_) => 10,
        Value::Date(_) => 13,
        // A composite sorts only against composites of its own type (ORDER BY is single-typed), so
        // this cross-family rank is only for totality; it sits after the scalar families.
        Value::Composite(_) => 11,
        // An array sorts only against arrays of its own element type (ORDER BY is single-typed), so
        // this cross-family rank is only for totality; it sits after composite.
        Value::Array(_) => 12,
        // A range sorts only against ranges of its own element type (ORDER BY is single-typed), so
        // this cross-family rank is only for totality; it sits after array.
        Value::Range(_) => 14,
        // json never sorts (42883 at resolve); jsonb sorts only against jsonb. Cross-family ranks
        // for totality only — they sit after the scalar/container families.
        Value::Json(_) => 15,
        Value::Jsonb(_) => 16,
        Value::JsonPath(_) => 17,
        // Poisoned (large-values.md §14): ORDER BY slots are in the touched set, so a sort
        // key is always resolved before it reaches the comparator.
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    // The function registry (extensibility.md §5) is data-driven over the generated catalog
    // tables, but two halves stay hand-written per core: the scalar kernel id (`scalar_func_id`)
    // and the result-code / plan interpreters. This guards against drift — a catalog row added
    // without a matching kernel id or with a result code no interpreter handles fails here, not
    // silently at some query's resolve.
    #[test]
    pub(crate) fn registry_covers_catalog() {
        for o in OPERATORS.iter().filter(|o| o.kind == "function") {
            if is_array_func_name(o.name) {
                // A polymorphic array function (array-functions.md §2): its kernel id comes from
                // `array_func_id` and its result is a reserved poly code or a scalar id.
                let _ = array_func_id(o.name);
                let concrete_array = o
                    .result
                    .strip_suffix("[]")
                    .is_some_and(|base| ScalarType::from_name(base).is_some());
                assert!(
                    o.result == "anyarray"
                        || o.result == "anyelement"
                        || concrete_array
                        || ScalarType::from_name(o.result).is_some(),
                    "array function {} has unhandled result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            if is_range_func_name(o.name) {
                // range_merge is the SET range function (range-functions.md §4): result `anyrange`,
                // and NO scalar accessor kernel (the resolver emits a RangeSetOp node, evaluated by
                // `eval_range_set_op`), so it skips `range_func_id` and the accessor result check.
                if o.name == "range_merge" {
                    assert_eq!(o.result, "anyrange", "range_merge result code");
                    continue;
                }
                // A polymorphic range accessor (range-functions.md §1): its kernel id comes from
                // `range_func_id` and its result is `anyelement` (the bound value) or `boolean`.
                let _ = range_func_id(o.name);
                assert!(
                    o.result == "anyelement" || ScalarType::from_name(o.result).is_some(),
                    "range function {} has unhandled result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            if is_range_ctor_name(o.name) {
                // A range constructor (range-functions.md §2): no scalar kernel id — the kernel is
                // `eval_range_ctor`, reached from the resolver. Its result is a concrete range id.
                assert!(
                    crate::range::range_by_name(o.result).is_some(),
                    "range constructor {} has non-range result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            if is_variadic_func_name(o.name) {
                // A VARIADIC function (array-functions.md §12): the count functions (num_nulls/
                // num_nonnulls) reach their kernel via `variadic_func_id`; the json builders
                // (json[b]_build_array/_object) reach theirs via `json_build_classify`. Either way
                // the result is a concrete scalar id (i32 / json / jsonb).
                if json_build_classify(o.name).is_none() {
                    let _ = variadic_func_id(o.name);
                }
                assert!(
                    ScalarType::from_name(o.result).is_some(),
                    "variadic function {} has unhandled result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            if matches!(
                o.name,
                "regexp_replace"
                    | "regexp_match"
                    | "regexp_like"
                    | "regexp_count"
                    | "regexp_substr"
                    | "regexp_instr"
            ) {
                // A regex scalar function (regex.md §8 / §8b): no scalar kernel id — the kernel is the
                // `RExpr::RegexFunc` eval, reached from `resolve_regex_func`. Its result is a scalar
                // (text / boolean / i32) or a concrete text[] code.
                let concrete_array = o
                    .result
                    .strip_suffix("[]")
                    .is_some_and(|b| ScalarType::from_name(b).is_some());
                assert!(
                    ScalarType::from_name(o.result).is_some() || concrete_array,
                    "regex function {} has unhandled result code {}",
                    o.name,
                    o.result
                );
                continue;
            }
            // Every function name maps to a kernel id (panics via unreachable! if not).
            let _ = scalar_func_id(o.name);
            // Every function result code is one the interpreter understands: "promoted" or a
            // literal scalar-type id.
            assert!(
                o.result == "promoted" || ScalarType::from_name(o.result).is_some(),
                "function {} has unhandled result code {}",
                o.name,
                o.result
            );
        }
        for a in AGGREGATES.iter() {
            assert!(
                matches!(
                    a.result,
                    "i64" | "decimal" | "sum_widen" | "same_as_input" | "jsonb" | "json"
                ),
                "aggregate {} has unhandled result code {}",
                a.name,
                a.result
            );
            // Every overload is reachable: a star row via `aggregate_has_star`, an expr row via
            // `lookup_aggregate_overload` over a representative operand of its declared family.
            if a.arg == "star" {
                assert!(aggregate_has_star(a.surface), "{} star overload", a.surface);
            } else {
                let probe = match a.arg_families.first().copied() {
                    Some("integer") => ResolvedType::Int(ScalarType::Int32),
                    Some("decimal") => ResolvedType::Decimal,
                    Some("float") => ResolvedType::Float(ScalarType::Float64),
                    _ => ResolvedType::Int(ScalarType::Int32), // "any"
                };
                let found = lookup_aggregate_overload(a.surface, &probe)
                    .expect("expr overload resolves for its declared family");
                // And its plan/result selection is total (panics via unreachable! otherwise).
                let lname = a.surface.to_ascii_lowercase();
                let _ = aggregate_plan(&lname, found.result, &probe);
            }
        }
    }

    /// The evaluator's per-operator cost base (functions.md §8): `operator_cost` returns each
    /// operator's catalog `cost` if authored, else the uniform `operator_eval`. Cross-checking
    /// against the generated `OPERATORS` table proves the lookup is data-driven — authoring a
    /// `cost` in catalog.toml is automatically honored, with no evaluator change. (The corpus
    /// cannot observe this while every weight is the uniform default — CLAUDE.md §10.)
    #[test]
    pub(crate) fn operator_cost_reflects_catalog() {
        for o in OPERATORS {
            let want = if o.cost == 0 {
                COSTS.operator_eval
            } else {
                o.cost
            };
            assert_eq!(operator_cost(o.name), want, "operator_cost({:?})", o.name);
        }
        // An unknown name falls back to the uniform operator_eval.
        assert_eq!(
            operator_cost("definitely_not_an_operator"),
            COSTS.operator_eval
        );
    }

    /// Every operator-enum → catalog-name mapping the evaluator charges through must resolve to a
    /// real catalog operator, so a typo in `op_name` / a wired literal is caught here, not silently
    /// masked by the uniform-weight fallback.
    #[test]
    pub(crate) fn wired_operator_names_exist_in_catalog() {
        let names = [
            CmpOp::Eq.op_name(),
            CmpOp::Ne.op_name(),
            CmpOp::Lt.op_name(),
            CmpOp::Gt.op_name(),
            CmpOp::Le.op_name(),
            CmpOp::Ge.op_name(),
            ArithOp::Add.op_name(),
            ArithOp::Sub.op_name(),
            ArithOp::Mul.op_name(),
            ArithOp::Div.op_name(),
            ArithOp::Mod.op_name(),
            "neg",
            "not",
            "and",
            "or",
        ];
        for name in names {
            assert!(
                OPERATORS.iter().any(|o| o.name == name),
                "wired operator name {name:?} is not in the catalog"
            );
        }
    }
}

#[cfg(test)]
mod skew_tests {
    // The slice-2d collation version-skew verdict (spec/design/collation.md §12/§14): the open-time
    // comparison + the read-only write-block. Skew has NO PostgreSQL analog (PG's collversion is the
    // opposite, host-OS-drift, §15), so it is a documented PG divergence verified by per-core unit
    // tests, not the oracle corpus (CLAUDE.md §10). White-box (it injects a file-pin/loaded-version
    // mismatch a public API cannot manufacture — a fresh file pins the loaded version). Mirrored by
    // impl/go/collation_host_test.go and impl/ts/tests/collation_host.test.ts.
    use super::*;
    use std::sync::Arc;

    pub(crate) fn load_bundle() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../spec/collation/fixtures/unicode.jucd");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read unicode.jucd: {e}"));
        crate::load_unicode_data(&bytes).expect("load unicode.jucd");
    }

    /// The pure verdict (`collation::version_skew`) — the cross-core contract (every core computes
    /// the identical result): same version as the loaded bundle ⇒ `None` (Full); a different pin ⇒
    /// `Some(loaded versions)` (Skewed); an unloaded name ⇒ `None` (the absent case is refused at
    /// open, not a skew verdict).
    #[test]
    pub(crate) fn version_skew_pure_verdict() {
        load_bundle();
        let loaded = crate::collation::loaded_collation("unicode").expect("unicode loaded");
        assert_eq!(
            crate::collation::version_skew(
                "unicode",
                &loaded.unicode_version,
                &loaded.cldr_version
            ),
            None,
            "same version is Full"
        );
        assert_eq!(
            crate::collation::version_skew("unicode", "0.0.0", "0"),
            Some((loaded.unicode_version.clone(), loaded.cldr_version.clone())),
            "a different pin is Skewed and reports the loaded version"
        );
        assert_eq!(
            crate::collation::version_skew("zz-not-loaded", "1", "1"),
            None,
            "an unloaded name yields no skew verdict (absent ⇒ refused at open)"
        );
    }

    /// A `unicode`-collated PK table is read-write while Full; once its `unicode` reference is pinned
    /// to a different version than the loaded bundle (the open-time state of a file built under an
    /// older bundle), the table degrades to **read-only**: reads still return the rows (the heap-scan
    /// fallback recomputes against the loaded table), every write raises `XX002`, and the skew is
    /// legible via `db.collations()`.
    #[test]
    pub(crate) fn skewed_collation_blocks_writes_reads_ok() {
        load_bundle();
        let mut db = Engine::new();
        crate::execute(
            &mut db,
            "CREATE TABLE t (x text COLLATE \"unicode\" PRIMARY KEY)",
        )
        .unwrap();
        crate::execute(&mut db, "INSERT INTO t VALUES ('b'), ('a')").unwrap();
        crate::execute(&mut db, "ANALYZE t (x)").unwrap();
        assert!(db.committed.column_statistics("t", 0).is_some());
        // Full so far: a write succeeds and every referenced collation reports Full.
        crate::execute(&mut db, "INSERT INTO t VALUES ('c')").unwrap();
        assert!(
            db.collations()
                .iter()
                .all(|c| c.verdict == CollationVerdict::Full),
            "all Full before skew injection"
        );

        // Inject skew: the file pinned `unicode` to an older version than the loaded bundle. This is
        // exactly the catalog state `Engine::open` produces for a file built under a prior bundle —
        // a catalog-local collation whose pin differs from the loaded set (collation.md §5/§12).
        let loaded = crate::collation::loaded_collation("unicode").unwrap();
        let mut skewed = (*loaded).clone();
        skewed.unicode_version = "0.0.0".to_string();
        db.committed.put_collation(Arc::new(skewed));
        // Persist and reopen the exact old-file/new-bundle state. Statistics values remain
        // structurally valid even though their old comparison ordering cannot be checked with the
        // loaded bundle; opening must succeed, and the estimator must not consume the facts.
        let image = db.to_image(8192, 1).expect("serialize skewed statistics");
        db = Engine::from_image(&image).expect("open skewed statistics");
        assert!(db.committed.column_statistics("t", 0).is_some());
        assert!(db.column_statistics_scoped(None, "t", 0).is_none());

        // The verdict is now Skewed and visible via introspection (the file's pin is reported).
        let info = db.collations();
        let uni = info
            .iter()
            .find(|c| c.name == "unicode")
            .expect("unicode referenced");
        assert_eq!(uni.verdict, CollationVerdict::Skewed);
        assert_eq!(uni.unicode_version, "0.0.0");

        // Reads still work — all three rows come back (values are version-independent §4.1).
        match crate::execute(&mut db, "SELECT x FROM t ORDER BY x COLLATE \"unicode\"").unwrap() {
            Outcome::Query { rows, .. } => assert_eq!(rows.len(), 3),
            other => panic!("expected rows, got {other:?}"),
        }

        // Every write is refused with XX002.
        for sql in [
            "INSERT INTO t VALUES ('d')",
            "UPDATE t SET x = 'z' WHERE x = 'a'",
            "DELETE FROM t WHERE x = 'a'",
            "CREATE INDEX t_x ON t (x)",
        ] {
            let err = crate::execute(&mut db, sql).expect_err(sql);
            assert_eq!(err.code(), "XX002", "{sql} must be XX002");
        }
    }

    /// The COLLATION UPGRADE migration (`db.upgrade_collations`, collation.md §12) clears the skew:
    /// after it the collation's pin is the loaded version, `db.collations()` reports Full, and the
    /// table is read-write again. Asserts the internal state the shared corpus
    /// (`suites/collation/collation_upgrade.test`) cannot read — the verdict-flip + the re-pin count —
    /// plus idempotence (a second upgrade re-pins nothing). The skew injection mirrors the test above.
    #[test]
    pub(crate) fn upgrade_clears_skew() {
        load_bundle();
        let mut db = Engine::new();
        crate::execute(
            &mut db,
            "CREATE TABLE t (x text COLLATE \"unicode\" PRIMARY KEY)",
        )
        .unwrap();
        crate::execute(&mut db, "INSERT INTO t VALUES ('b'), ('a')").unwrap();
        crate::execute(&mut db, "ANALYZE t (x)").unwrap();
        assert!(db.committed.column_statistics("t", 0).is_some());
        // Inject skew (a file built under a prior bundle), as in the test above.
        let loaded = crate::collation::loaded_collation("unicode").unwrap();
        let mut skewed = (*loaded).clone();
        skewed.unicode_version = "0.0.0".to_string();
        db.committed.put_collation(Arc::new(skewed));
        assert_eq!(
            db.collations()
                .iter()
                .find(|c| c.name == "unicode")
                .unwrap()
                .verdict,
            CollationVerdict::Skewed,
            "skewed before upgrade"
        );

        // The migration re-pins the one skewed collation and rebuilds its (collated PK) table.
        assert_eq!(
            db.upgrade_collations().unwrap(),
            1,
            "one collation re-pinned"
        );

        // The verdict is now Full, the pin advanced to the loaded version, and writes succeed again.
        let uni = db
            .collations()
            .into_iter()
            .find(|c| c.name == "unicode")
            .expect("unicode referenced");
        assert_eq!(uni.verdict, CollationVerdict::Full, "Full after upgrade");
        assert_eq!(uni.unicode_version, loaded.unicode_version);
        assert!(
            db.committed.column_statistics("t", 0).is_none(),
            "upgrade clears facts ordered under the old collation"
        );
        crate::execute(&mut db, "INSERT INTO t VALUES ('c')").expect("writable after upgrade");

        // Idempotent: nothing is skewed now, so a second upgrade re-pins zero collations.
        assert_eq!(db.upgrade_collations().unwrap(), 0, "idempotent no-op");
    }
}
