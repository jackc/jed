package jed

import (
	"bytes"
	"math/big"
	"slices"
	"sort"
	"strings"
)

// Window-frame evaluation, row ordering, and three-valued comparison — the runtime that turns a
// resolved window/ORDER BY plan into ordered, framed rows. This file holds: the boolean 3VL
// combinators (or3/not3); the generic ORDER BY sorters (sortRows/sortRowsCollated and the
// decorated/collated row comparators); frameCtx and its ROWS/GROUPS/RANGE frame-bound arithmetic;
// applyWindowStage (the per-partition window driver); and the value ordering primitives
// (keyCmp/valueCmp/familyRank) shared by sorting and framing.

// or3 is three-valued OR (Kleene): used to build <= / >= from < / > and =, so a NULL
// operand yields UNKNOWN rather than a wrong FALSE (CLAUDE.md §4).
func or3(a, b ThreeValued) ThreeValued {
	if a == True || b == True {
		return True
	}
	if a == Unknown || b == Unknown {
		return Unknown
	}
	return False
}

// not3 is the Kleene NOT: True<->False, Unknown stays Unknown. Used to build `<>` as the
// negation of `=`, so a NULL operand still yields UNKNOWN (`NULL <> NULL`), not a wrong True.
func not3(a ThreeValued) ThreeValued {
	switch a {
	case True:
		return False
	case False:
		return True
	default: // Unknown
		return Unknown
	}
}

// sortRows sorts rows by the ORDER BY keys (spec/design/grammar.md §10). The all-C fast path is a
// stable sort over the value comparator; if ANY key carries a collation, the collation-aware
// sortRowsCollated decorate sorter runs instead (it can fail — an unmapped code point is 0A000).
// The row type is generic over Row (the scan path) and [][]Value (the setop / aggregate paths):
// both have the core type []Value, so a single comparator family serves every sort site.
func sortRows[R ~[]Value](rows []R, order []orderSlot) error {
	for _, k := range order {
		if k.collation != nil {
			return sortRowsCollated(rows, order)
		}
	}
	sort.SliceStable(rows, func(a, b int) bool { return cmpRowsByOrder(rows[a], rows[b], order) < 0 })
	return nil
}

// offsetCount is the integer count of a ROWS/GROUPS offset bound (ValInt by construction). It is
// clamped to [0, np] before index arithmetic so a huge literal offset cannot overflow int — any
// offset >= np already saturates the bound to the partition edge. Mirrors Rust's i128 widening.
func offsetCount(v Value, np int) int {
	if v.Int > int64(np) {
		return np
	}
	return int(v.Int)
}

// rangeVVsBound returns the sign of v - (cur ∓ off) for a RANGE value offset (window.md §6),
// computed exactly: integer keys use math/big so the bound never overflows int64 (matching Rust's
// i128 / TS's bigint); decimal keys use exact decimal arithmetic; float keys widen to f64 and
// compute the bound with the in-contract correctly-rounded +/- kernel (float.md §5 — bit-identical
// cross-core), comparing with the PG float total order (floatTotalCmp). The total order reproduces
// PG's in_range NaN handling for free: a NaN current key makes the bound NaN (NaN ∓ finite = NaN), so
// a NaN row equals it and any non-NaN row is below it, while a NaN row against a non-NaN bound sorts
// above. The offset is always finite (an int offset, or a decimal one that would otherwise overflow
// already trapped at resolve), so cur ∓ off never produces NaN itself. subtract chooses cur - off vs
// cur + off. Mirrors Rust's range_v_vs_bound.
func rangeVVsBound(v, cur, off Value, subtract bool) (int, error) {
	if cur.Kind == ValInt {
		b := big.NewInt(cur.Int)
		if subtract {
			b.Sub(b, big.NewInt(off.Int))
		} else {
			b.Add(b, big.NewInt(off.Int))
		}
		return big.NewInt(v.Int).Cmp(b), nil
	}
	// Float key: widen to f64 (PG computes in_range_float*_float8's sum in float8 even for an f32
	// key) and compare in the float total order.
	if cur.IsFloat() {
		c := cur.asF64()
		x := v.asF64()
		var b float64
		if subtract {
			b = c - off.F64()
		} else {
			b = c + off.F64()
		}
		return floatTotalCmp(x, b), nil
	}
	var (
		b   Decimal
		err error
	)
	if subtract {
		b, err = cur.decimal().Sub(*off.decimal())
	} else {
		b, err = cur.decimal().Add(*off.decimal())
	}
	if err != nil {
		return 0, err
	}
	return v.decimal().CmpValue(b), nil
}

// frameCtx holds one partition's peer-group structure (window.md §3/§6), shared across every row's
// frame lookup. Peers are rows equal on the window ORDER BY keys; peerStart/peerEnd bracket each
// row's peer group, groupOf is its peer-group ordinal, and groupSpans lists every group's [start,
// end). Mirrors Rust's FrameCtx.
type frameCtx struct {
	ordered    []int
	rows       []storedRow
	order      []orderSlot
	np         int
	peerStart  []int
	peerEnd    []int
	groupOf    []int
	groupSpans [][2]int
}

func newFrameCtx(ordered []int, rows []storedRow, order []orderSlot, collKeys [][][]byte) *frameCtx {
	np := len(ordered)
	var groupSpans [][2]int
	s := 0
	for pos := 1; pos < np; pos++ {
		if cmpWindowRows(ordered[pos], ordered[s], rows, order, collKeys) != 0 {
			groupSpans = append(groupSpans, [2]int{s, pos})
			s = pos
		}
	}
	if np > 0 {
		groupSpans = append(groupSpans, [2]int{s, np})
	}
	peerStart := make([]int, np)
	peerEnd := make([]int, np)
	groupOf := make([]int, np)
	for gi, span := range groupSpans {
		for p := span[0]; p < span[1]; p++ {
			peerStart[p] = span[0]
			peerEnd[p] = span[1]
			groupOf[p] = gi
		}
	}
	return &frameCtx{ordered, rows, order, np, peerStart, peerEnd, groupOf, groupSpans}
}

// bounds returns the [lo, hi) frame for the row at sorted position pos (window.md §6). A nil frame
// ⇒ the default frame (RANGE UNBOUNDED PRECEDING TO CURRENT ROW = [0, peerEnd)).
func (c *frameCtx) bounds(pos int, frame *resolvedFrame) (int, int, error) {
	if frame == nil {
		return 0, c.peerEnd[pos], nil
	}
	switch frame.mode {
	case frameRows:
		lo, hi := c.rowsBounds(pos, frame)
		return lo, hi, nil
	case frameGroups:
		lo, hi := c.groupsBounds(pos, frame)
		return lo, hi, nil
	default: // FrameRange
		return c.rangeBounds(pos, frame)
	}
}

// isExcluded reports whether sorted position k is dropped from the current row pos's frame by
// EXCLUDE (window.md §6): CURRENT ROW drops the row itself, GROUP its whole peer group, TIES the
// peers but not the row, NO OTHERS nothing. Exclusion removes only rows already in [lo, hi).
// Mirrors Rust's FrameCtx::is_excluded.
func (c *frameCtx) isExcluded(pos, k int, exclude frameExclusion) bool {
	switch exclude {
	case frameExcludeCurrentRow:
		return k == pos
	case frameExcludeGroup:
		return c.peerStart[pos] <= k && k < c.peerEnd[pos]
	case frameExcludeTies:
		return k != pos && c.peerStart[pos] <= k && k < c.peerEnd[pos]
	default: // FrameExcludeNoOthers
		return false
	}
}

// frameExclusion returns a resolved frame's exclusion, or NoOthers for the default (nil) frame.
func newFrameExclusion(frame *resolvedFrame) frameExclusion {
	if frame == nil {
		return frameExcludeNoOthers
	}
	return frame.exclude
}

// rowsBounds: physical row offsets in the partition sequence; bounds clamp to [0, np].
func (c *frameCtx) rowsBounds(pos int, f *resolvedFrame) (int, int) {
	np := c.np
	lo := 0
	switch f.start.kind {
	case boundUnboundedPreceding:
		lo = 0
	case boundPreceding:
		lo = pos - offsetCount(f.start.offVal, np)
	case boundCurrentRow:
		lo = pos
	case boundFollowing:
		lo = pos + offsetCount(f.start.offVal, np)
	case boundUnboundedFollowing:
		lo = np
	}
	hi := 0
	switch f.end.kind {
	case boundUnboundedPreceding:
		hi = 0
	case boundPreceding:
		hi = pos - offsetCount(f.end.offVal, np) + 1
	case boundCurrentRow:
		hi = pos + 1
	case boundFollowing:
		hi = pos + offsetCount(f.end.offVal, np) + 1
	case boundUnboundedFollowing:
		hi = np
	}
	lo = clampIdx(lo, np)
	hi = clampIdx(hi, np)
	if hi < lo {
		hi = lo
	}
	return lo, hi
}

// groupsBounds: peer-group offsets — a bound g PRECEDING/FOLLOWING lands on the cg ∓ g-th peer
// group's start (a start bound) or end (an end bound); a group index below 0 clamps to the
// partition start, at or above the group count to the partition end.
func (c *frameCtx) groupsBounds(pos int, f *resolvedFrame) (int, int) {
	np := c.np
	cg := c.groupOf[pos]
	g := len(c.groupSpans)
	startAt := func(j int) int {
		if j < 0 {
			return 0
		}
		if j >= g {
			return np
		}
		return c.groupSpans[j][0]
	}
	endAt := func(j int) int {
		if j < 0 {
			return 0
		}
		if j >= g {
			return np
		}
		return c.groupSpans[j][1]
	}
	lo := 0
	switch f.start.kind {
	case boundUnboundedPreceding:
		lo = 0
	case boundPreceding:
		lo = startAt(cg - offsetCount(f.start.offVal, np))
	case boundCurrentRow:
		lo = startAt(cg)
	case boundFollowing:
		lo = startAt(cg + offsetCount(f.start.offVal, np))
	case boundUnboundedFollowing:
		lo = np
	}
	hi := 0
	switch f.end.kind {
	case boundUnboundedPreceding:
		hi = 0
	case boundPreceding:
		hi = endAt(cg - offsetCount(f.end.offVal, np))
	case boundCurrentRow:
		hi = endAt(cg)
	case boundFollowing:
		hi = endAt(cg + offsetCount(f.end.offVal, np))
	case boundUnboundedFollowing:
		hi = np
	}
	if hi < lo {
		hi = lo
	}
	return lo, hi
}

// rangeBounds: logical offsets on the single ordering-key value (window.md §6). A bound with no
// offset (UNBOUNDED / CURRENT ROW) is peer/edge based and needs no key arithmetic. With a value
// offset, the frame spans the rows whose key is within the offset of the current key; a NULL
// current key has only its NULL peers (offset/CURRENT bounds collapse to the peer group, the PG
// rule), while UNBOUNDED bounds still reach the partition edge.
func (c *frameCtx) rangeBounds(pos int, f *resolvedFrame) (int, int, error) {
	np := c.np
	startOff := f.start.kind == boundPreceding || f.start.kind == boundFollowing
	endOff := f.end.kind == boundPreceding || f.end.kind == boundFollowing
	if !startOff && !endOff {
		lo := 0
		if f.start.kind != boundUnboundedPreceding { // CurrentRow
			lo = c.peerStart[pos]
		}
		hi := np
		if f.end.kind != boundUnboundedFollowing { // CurrentRow
			hi = c.peerEnd[pos]
		}
		if hi < lo {
			hi = lo
		}
		return lo, hi, nil
	}
	// Offset present ⇒ exactly one ORDER BY key (validated at resolve).
	col := c.order[0].idx
	desc := c.order[0].descending
	cur := c.rows[c.ordered[pos]][col]
	if cur.IsNull() {
		lo := 0
		if f.start.kind != boundUnboundedPreceding {
			lo = c.peerStart[pos]
		}
		hi := np
		if f.end.kind != boundUnboundedFollowing {
			hi = c.peerEnd[pos]
		}
		if hi < lo {
			hi = lo
		}
		return lo, hi, nil
	}
	var (
		lo  int
		err error
	)
	switch f.start.kind {
	case boundUnboundedPreceding:
		lo = 0
	case boundCurrentRow:
		lo = c.peerStart[pos]
	case boundPreceding:
		lo, err = c.rangeStart(col, cur, f.start.offVal, true, desc)
	case boundFollowing:
		lo, err = c.rangeStart(col, cur, f.start.offVal, false, desc)
	case boundUnboundedFollowing:
		lo = np
	}
	if err != nil {
		return 0, 0, err
	}
	hi := np
	switch f.end.kind {
	case boundUnboundedFollowing:
		hi = np
	case boundCurrentRow:
		hi = c.peerEnd[pos]
	case boundPreceding:
		hi, err = c.rangeEnd(col, cur, f.end.offVal, true, desc, lo)
	case boundFollowing:
		hi, err = c.rangeEnd(col, cur, f.end.offVal, false, desc, lo)
	case boundUnboundedPreceding:
		hi = 0
	}
	if err != nil {
		return 0, 0, err
	}
	if hi < lo {
		hi = lo
	}
	return lo, hi, nil
}

// rangeStart is the first sorted position whose key satisfies a RANGE start bound (NULL keys never
// qualify for a non-NULL current row). subtract = isPreceding XOR descending chooses the bound side.
func (c *frameCtx) rangeStart(col int, cur, off Value, isPreceding, desc bool) (int, error) {
	subtract := isPreceding != desc
	for i := 0; i < c.np; i++ {
		v := c.rows[c.ordered[i]][col]
		if v.IsNull() {
			continue
		}
		ord, err := rangeVVsBound(v, cur, off, subtract)
		if err != nil {
			return 0, err
		}
		// ascending frame: v >= bound; descending frame: v <= bound.
		include := ord >= 0
		if desc {
			include = ord <= 0
		}
		if include {
			return i, nil
		}
	}
	return c.np, nil
}

// rangeEnd is the exclusive end of a RANGE end bound, scanning forward from lo while the key stays
// in frame (the in-frame keys form a contiguous run over the sorted partition).
func (c *frameCtx) rangeEnd(col int, cur, off Value, isPreceding, desc bool, lo int) (int, error) {
	subtract := isPreceding != desc
	hi := lo
	for i := lo; i < c.np; i++ {
		v := c.rows[c.ordered[i]][col]
		if v.IsNull() {
			break
		}
		ord, err := rangeVVsBound(v, cur, off, subtract)
		if err != nil {
			return 0, err
		}
		// ascending frame: v <= bound; descending frame: v >= bound.
		include := ord <= 0
		if desc {
			include = ord >= 0
		}
		if include {
			hi = i + 1
		} else {
			break
		}
	}
	return hi, nil
}

// clampIdx clamps an index into [0, np].
func clampIdx(x, np int) int {
	if x < 0 {
		return 0
	}
	if x > np {
		return np
	}
	return x
}

// applyWindowStage runs the WINDOW stage (spec/design/window.md §5.2): for each window function,
// partition the rows, sort each partition by the window ORDER BY (stable → PK tie-break, as rows
// arrive in PK scan order), compute the per-row result, and APPEND it to every row (so window
// result i lands at flat slot input_width+i, where the projection reads it). The partition + sort
// are unmetered (like ORDER BY / GROUP BY); each computed result charges window_result and guards
// the ceiling. S0: row_number() only; partitions bucket value-canonically via an insertion-ordered
// list, so no map iteration order leaks (CLAUDE.md §8/§10).
//
// The frame-sensitive plans (aggregate windows, first/last/nth_value) use a frameCtx, which
// precomputes the partition's peer-group structure once and maps each row to its [lo, hi) frame.
// spec/design/window.md §6.
// materializeOrderExprs materializes the general-expression ORDER BY keys before the sort
// (spec/design/grammar.md §10): for each row evaluate every orderExprs[k] and append the value, so its
// sort slot final_width+k reads the appended column and the slot-based comparator stays unchanged —
// the exact mechanism a non-column window key uses (window.md §5.1, applyWindowStage). Runs over every
// pre-sort row (before LIMIT, since the sort needs them all); the per-row evaluation is metered like a
// projection (operator_eval per node, charged inside eval). A no-op — and zero added cost — when
// orderExprs is empty (a column/ordinal-only ORDER BY, byte-identical to before).
func materializeOrderExprs(rows []storedRow, orderExprs []*rExpr, env *evalEnv, meter *costMeter) error {
	if len(orderExprs) == 0 {
		return nil
	}
	for i := range rows {
		vals := make([]Value, len(orderExprs))
		for k, oe := range orderExprs {
			v, err := oe.eval(rows[i], env, meter)
			if err != nil {
				return err
			}
			vals[k] = v
		}
		rows[i] = append(rows[i], vals...)
	}
	return nil
}

func applyWindowStage(rows []storedRow, specs []windowSpec, windowKeys []*rExpr, env *evalEnv, meter *costMeter) error {
	n := len(rows)
	if n == 0 {
		return nil
	}
	// Materialize the non-column PARTITION BY / ORDER BY key expressions (window.md §5.1): evaluate
	// each against the row and append it, so a materialized key's slot input_width+k reads the
	// appended column and the partition / sort / frame machinery below (all slot-based) is unchanged.
	// The window results are appended AFTER these, so a result slot is input_width+len(windowKeys)+w
	// (the rebased projection slot). Empty for a column-only window — no appended columns, the result
	// slot stays input_width+w, byte-identical to before. The key evaluation is metered like any
	// expression (operator_eval per node): new, deterministic, cross-core-identical work that exists
	// only for an expression key (a bare-column key is not in windowKeys).
	if len(windowKeys) > 0 {
		for i := range rows {
			kv := make([]Value, len(windowKeys))
			for k, ke := range windowKeys {
				v, err := ke.eval(rows[i], env, meter)
				if err != nil {
					return err
				}
				kv[k] = v
			}
			rows[i] = append(rows[i], kv...)
		}
	}
	// The shared partition/sort pass (window.md §5.2): specs that share an identical PARTITION BY +
	// ORDER BY are partitioned and sorted ONCE (the expensive step), then each computes its own
	// results over the shared sorted partitions. The partition + sort are unmetered (§8), so this is
	// purely a wall-clock win — the per-spec result/frame metering, and thus the cost, are unchanged.
	groups := groupWindowSpecs(specs)
	specGroup := make([]int, len(specs))
	type groupShared struct {
		partitions [][]int
		collKeys   [][][]byte
	}
	cache := make([]groupShared, len(groups))
	for gi, group := range groups {
		rep := specs[group[0]]
		for _, si := range group {
			specGroup[si] = gi
		}
		// Partition the row indices by the partition-key values. The map is an index only (never
		// iterated); output comes from the insertion-ordered `partitions` (no map-order leak). The
		// key is the value-canonical distinctRowKey (collapses 1.5/1.50, groups NULL with NULL).
		index := make(map[string]int)
		var partitions [][]int
		for i := range rows {
			key := make([]Value, len(rep.partition))
			for j, p := range rep.partition {
				key[j] = rows[i][p]
			}
			k := distinctRowKey(key)
			pi, ok := index[k]
			if !ok {
				pi = len(partitions)
				index[k] = pi
				partitions = append(partitions, nil)
			}
			partitions[pi] = append(partitions[pi], i)
		}
		// Collated UCA sort-key bytes for the shared order's collated slots (window.md §3/§5); nil
		// when no key is collated, an unmapped code point fails 0A000 here.
		collKeys, err := windowCollKeys(rows, rep.order)
		if err != nil {
			return err
		}
		// Sort each partition by the shared window ORDER BY. SliceStable keeps a full tie at
		// ascending original index = PK scan order (the §3 PK tie-break).
		if len(rep.order) > 0 {
			for _, part := range partitions {
				sort.SliceStable(part, func(a, b int) bool {
					return cmpWindowRows(part[a], part[b], rows, rep.order, collKeys) < 0
				})
			}
		}
		cache[gi] = groupShared{partitions: partitions, collKeys: collKeys}
	}
	for si := range specs {
		spec := specs[si]
		shared := cache[specGroup[si]]
		collKeys := shared.collKeys
		// Compute each row's result into a per-row slot, then append in input order.
		results := make([]Value, n)
		for i := range results {
			results[i] = NullValue()
		}
		for _, ordered := range shared.partitions {
			switch spec.plan {
			case planRowNumber:
				for pos, ri := range ordered {
					if err := meter.Guard(); err != nil { // enforce the cost ceiling per result (CLAUDE.md §13)
						return err
					}
					meter.Charge(costs.WindowResult)
					results[ri] = IntValue(int64(pos) + 1)
				}
			case planRank, planDenseRank, planPercentRank, planCumeDist:
				// Peer-aware ranking (window.md §3/§4): peers are rows EQUAL on the window ORDER BY
				// keys only. A single pass identifies peer-group spans [start, end) over the sorted
				// partition; an empty ORDER BY makes the whole partition one peer group. rank =
				// start+1, dense_rank = group ordinal, percent_rank = start/(N-1) (0 if N=1),
				// cume_dist = end/N. The ratios are f64 (PG's float8, window.md §4): one IEEE
				// correctly-rounded division of small integers that convert exactly to binary64, so
				// the value is bit-identical across cores and to PG (the in-contract kernel, float.md §5).
				np := len(ordered)
				type span struct{ start, end int }
				var groups []span
				s := 0
				for pos := 1; pos < np; pos++ {
					if cmpWindowRows(ordered[pos], ordered[s], rows, spec.order, collKeys) != 0 {
						groups = append(groups, span{s, pos})
						s = pos
					}
				}
				if np > 0 {
					groups = append(groups, span{s, np})
				}
				for gi, g := range groups {
					for _, ri := range ordered[g.start:g.end] {
						if err := meter.Guard(); err != nil {
							return err
						}
						meter.Charge(costs.WindowResult)
						switch spec.plan {
						case planRank:
							results[ri] = IntValue(int64(g.start) + 1)
						case planDenseRank:
							results[ri] = IntValue(int64(gi) + 1)
						case planPercentRank:
							if np <= 1 {
								results[ri] = Float64Value(0.0)
							} else {
								results[ri] = Float64Value(float64(g.start) / float64(np-1))
							}
						default: // planCumeDist
							results[ri] = Float64Value(float64(g.end) / float64(np))
						}
					}
				}
			case planNtile:
				// ntile(n): distribute the partition into n ranked buckets, larger buckets first
				// (window.md §4). n is evaluated once (the first sorted row); NULL n → NULL for all;
				// n <= 0 → 22014. Position-based: bucket boundaries are by sorted position, not peers.
				np := len(ordered)
				nv, err := spec.args[0].eval(rows[ordered[0]], env, meter)
				if err != nil {
					return err
				}
				switch {
				case nv.IsNull():
					// NULL bucket count → NULL for every row (PG).
					for _, ri := range ordered {
						if err := meter.Guard(); err != nil {
							return err
						}
						meter.Charge(costs.WindowResult)
						results[ri] = NullValue()
					}
				default:
					nbuckets := nv.Int
					if nbuckets <= 0 {
						return newError(InvalidArgumentForNtile, "argument of ntile must be greater than zero")
					}
					nb := int(nbuckets)
					base := np / nb         // floor rows per bucket
					rem := np % nb          // the first `rem` buckets get one extra row
					big := rem * (base + 1) // rows in the larger (base+1) buckets
					for pos, ri := range ordered {
						if err := meter.Guard(); err != nil {
							return err
						}
						meter.Charge(costs.WindowResult)
						// Larger buckets first: positions [0, big) → (base+1)-sized buckets, the rest
						// → base-sized buckets. `base` is 0 only when nbuckets > np, and then every
						// pos < big so the else branch never divides by 0.
						var bucket int
						if pos < big {
							bucket = pos/(base+1) + 1
						} else {
							bucket = rem + (pos-big)/base + 1
						}
						results[ri] = IntValue(int64(bucket))
					}
				}
			case planLag, planLead:
				// lag/lead (window.md §4): the value `offset` positions back (lag) / forward (lead)
				// in the partition, else the default (or NULL). Frame-insensitive — offset is by
				// sorted position. The value is evaluated for every row; offset once (NULL → all
				// NULL); the default per out-of-range row.
				np := len(ordered)
				vals := make([]Value, np)
				for pos, ri := range ordered {
					v, err := spec.args[0].eval(rows[ri], env, meter)
					if err != nil {
						return err
					}
					vals[pos] = v
				}
				// offset: evaluated once from the first sorted row; NULL → NULL for every row;
				// absent → 1. A negative offset reverses the direction (lag(v,-1) acts like lead).
				var offset int64
				offsetNull := false
				if len(spec.args) >= 2 {
					ov, err := spec.args[1].eval(rows[ordered[0]], env, meter)
					if err != nil {
						return err
					}
					if ov.IsNull() {
						offsetNull = true
					} else {
						offset = ov.Int
					}
				} else {
					offset = 1
				}
				var dir int64 = -1
				if spec.plan == planLead {
					dir = 1
				}
				for pos, ri := range ordered {
					if err := meter.Guard(); err != nil {
						return err
					}
					meter.Charge(costs.WindowResult)
					switch {
					case offsetNull:
						results[ri] = NullValue()
					default:
						target := int64(pos) + dir*offset
						if target >= 0 && target < int64(np) {
							// vals[target] is a Value; copy it (the slot is reassigned per row).
							v := vals[target]
							results[ri] = v
						} else if len(spec.args) == 3 {
							v, err := spec.args[2].eval(rows[ri], env, meter)
							if err != nil {
								return err
							}
							results[ri] = v
						} else {
							results[ri] = NullValue()
						}
					}
				}
			case planAgg:
				np := len(ordered)
				hasOperand := len(spec.args) > 0 // COUNT(*) has no operand
				// opval evaluates the aggregate operand at sorted position k (NULL for COUNT(*)).
				opval := func(k int) (Value, error) {
					if hasOperand {
						return spec.args[0].eval(rows[ordered[k]], env, meter)
					}
					return NullValue(), nil
				}
				// filterPass evaluates the FILTER (WHERE cond) at sorted position k: a frame row whose
				// filter is not TRUE does not fold into the window aggregate (aggregates.md §20). Charged
				// per visited frame row (its operator_evals); a nil filter keeps every row. A FILTER forces
				// the naive re-fold path for explicit frames (a filtered row cannot be cleanly un-folded).
				filterPass := func(k int) (bool, error) {
					if spec.filter == nil {
						return true, nil
					}
					v, err := spec.filter.eval(rows[ordered[k]], env, meter)
					if err != nil {
						return false, err
					}
					return v.IsTrue(), nil
				}
				if spec.frame == nil {
					// DEFAULT frame (window.md §6): RANGE UNBOUNDED PRECEDING TO CURRENT ROW with a
					// window ORDER BY (a RUNNING aggregate — CURRENT ROW spans the current peer
					// group), or the WHOLE partition with no ORDER BY. Both reduce to the same shape:
					// a single running pass, snapshotting the running acc at each peer-group boundary
					// (no ORDER BY → one peer group → one whole-partition value) — O(n).
					type span struct{ start, end int }
					var groups []span
					s := 0
					for pos := 1; pos < np; pos++ {
						if cmpWindowRows(ordered[pos], ordered[s], rows, spec.order, collKeys) != 0 {
							groups = append(groups, span{s, pos})
							s = pos
						}
					}
					if np > 0 {
						groups = append(groups, span{s, np})
					}
					a := newAcc(spec.aggPlan)
					for _, g := range groups {
						for k := g.start; k < g.end; k++ {
							// The frame fold work (window.md §8) — metered so a running aggregate over
							// a large partition stays cost-bounded.
							meter.Charge(costs.WindowFrameStep)
							pass, err := filterPass(k)
							if err != nil {
								return err
							}
							if !pass {
								continue // FILTER excludes this row from the running fold
							}
							v, err := opval(k)
							if err != nil {
								return err
							}
							if err := a.fold(v, meter); err != nil {
								return err
							}
						}
						// Snapshot the running accumulator for this peer group's frame [0, end) — the
						// clone keeps the running acc going (deep-copied float buffer).
						out, err := a.clone().finalize()
						if err != nil {
							return err
						}
						for _, ri := range ordered[g.start:g.end] {
							if err := meter.Guard(); err != nil {
								return err
							}
							meter.Charge(costs.WindowResult)
							results[ri] = out
						}
					}
				} else {
					// EXPLICIT frame (window.md §5.2/§6). The sorted partition makes the frame bounds
					// [lo, hi) monotonic non-decreasing in pos, so a NO-EXCLUDE aggregate CARRIES one
					// accumulator across rows rather than re-folding each frame from scratch (the
					// sliding-window optimization):
					//   • an EXPANDING frame (start UNBOUNDED PRECEDING ⇒ lo ≡ 0) folds each entering
					//     row once as hi advances — byte-identical for EVERY aggregate (fold order is
					//     the sorted-prefix order the naive path uses) — O(n);
					//   • a MOVING frame additionally UN-folds the rows leaving on the left, but only
					//     for the exactly-invertible COUNT / COUNT(*) — O(n);
					//   • a MOVING frame over SUM/AVG/MIN/MAX/float (not safely invertible) and ANY
					//     frame with EXCLUDE re-fold from scratch (the naive O(partition²)).
					// window_frame_step is charged per folded AND per un-folded row, so it only LOWERS;
					// each row's operand is evaluated at most once (cached in vals), so operator_eval
					// never rises.
					ctx := newFrameCtx(ordered, rows, spec.order, collKeys)
					exclude := newFrameExclusion(spec.frame)
					vals := make([]Value, np)
					valSet := make([]bool, np)
					evalAt := func(k int) (Value, error) {
						if !hasOperand {
							return NullValue(), nil
						}
						if !valSet[k] {
							v, err := spec.args[0].eval(rows[ordered[k]], env, meter)
							if err != nil {
								return NullValue(), err
							}
							vals[k], valSet[k] = v, true
						}
						return vals[k], nil
					}
					if exclude != frameExcludeNoOthers || spec.filter != nil {
						// EXCLUDE or FILTER breaks the clean add/remove model → naive per-row re-fold
						// (dropped rows are neither metered nor counted), over the cached operand. A
						// FILTER additionally skips a non-TRUE frame row.
						for pos := 0; pos < np; pos++ {
							lo, hi, err := ctx.bounds(pos, spec.frame)
							if err != nil {
								return err
							}
							a := newAcc(spec.aggPlan)
							for k := lo; k < hi; k++ {
								if ctx.isExcluded(pos, k, exclude) {
									continue
								}
								meter.Charge(costs.WindowFrameStep)
								pass, err := filterPass(k)
								if err != nil {
									return err
								}
								if !pass {
									continue
								}
								v, err := evalAt(k)
								if err != nil {
									return err
								}
								if err := a.fold(v, meter); err != nil {
									return err
								}
							}
							if err := meter.Guard(); err != nil {
								return err
							}
							meter.Charge(costs.WindowResult)
							out, err := a.finalize()
							if err != nil {
								return err
							}
							results[ordered[pos]] = out
						}
					} else {
						// SLIDING (monotone carry). removable aggregates un-fold the left edge; the rest
						// rebuild when lo advances (an expanding frame never advances lo, so it only adds).
						removable := spec.aggPlan == planCountStar || spec.aggPlan == planCount
						a := newAcc(spec.aggPlan)
						curLo, curHi := 0, 0
						for pos := 0; pos < np; pos++ {
							lo, hi, err := ctx.bounds(pos, spec.frame)
							if err != nil {
								return err
							}
							if !removable && lo > curLo {
								// Left edge advanced over a non-invertible aggregate ⇒ rebuild over [lo, hi).
								a = newAcc(spec.aggPlan)
								for k := lo; k < hi; k++ {
									meter.Charge(costs.WindowFrameStep)
									v, err := evalAt(k)
									if err != nil {
										return err
									}
									if err := a.fold(v, meter); err != nil {
										return err
									}
								}
							} else {
								// Un-fold rows leaving on the left (invertible only; empty when lo == curLo) …
								remHi := lo
								if curHi < remHi {
									remHi = curHi
								}
								for k := curLo; k < remHi; k++ {
									meter.Charge(costs.WindowFrameStep)
									v, err := evalAt(k)
									if err != nil {
										return err
									}
									a.unfold(v, meter)
								}
								// … and fold rows entering on the right.
								addLo := curHi
								if lo > addLo {
									addLo = lo
								}
								for k := addLo; k < hi; k++ {
									meter.Charge(costs.WindowFrameStep)
									v, err := evalAt(k)
									if err != nil {
										return err
									}
									if err := a.fold(v, meter); err != nil {
										return err
									}
								}
							}
							curLo, curHi = lo, hi
							if err := meter.Guard(); err != nil {
								return err
							}
							meter.Charge(costs.WindowResult)
							out, err := a.clone().finalize()
							if err != nil {
								return err
							}
							results[ordered[pos]] = out
						}
					}
				}
			case planFirstValue, planLastValue, planNthValue:
				// Frame-sensitive value pickers (S4, window.md §4): first/last/nth row of the frame.
				np := len(ordered)
				// The value expression, evaluated once per row (sorted order).
				vals := make([]Value, np)
				for pos, ri := range ordered {
					v, err := spec.args[0].eval(rows[ri], env, meter)
					if err != nil {
						return err
					}
					vals[pos] = v
				}
				// nth_value's position — evaluated once; NULL → NULL for all; < 1 → 22016.
				var nth int // the 1-based position (0 unused for first/last)
				nthNull := false
				if spec.plan == planNthValue {
					nv, err := spec.args[1].eval(rows[ordered[0]], env, meter)
					if err != nil {
						return err
					}
					if nv.IsNull() {
						nthNull = true
					} else if nv.Int >= 1 {
						nth = int(nv.Int)
					} else {
						return newError(InvalidArgumentForNthValue, "argument of nth_value must be greater than zero")
					}
				}
				ctx := newFrameCtx(ordered, rows, spec.order, collKeys)
				exclude := newFrameExclusion(spec.frame)
				for pos := 0; pos < np; pos++ {
					if err := meter.Guard(); err != nil {
						return err
					}
					meter.Charge(costs.WindowResult)
					lo, hi, err := ctx.bounds(pos, spec.frame)
					if err != nil {
						return err
					}
					// first/last/nth pick over the frame's NON-excluded rows (window.md §6); the
					// NoOthers fast path breaks on the first row, so it stays O(1).
					out := NullValue()
					switch spec.plan {
					case planFirstValue:
						for k := lo; k < hi; k++ {
							if !ctx.isExcluded(pos, k, exclude) {
								out = vals[k]
								break
							}
						}
					case planLastValue:
						for k := hi - 1; k >= lo; k-- {
							if !ctx.isExcluded(pos, k, exclude) {
								out = vals[k]
								break
							}
						}
					default: // planNthValue
						if !nthNull {
							count := 0
							for k := lo; k < hi; k++ {
								if ctx.isExcluded(pos, k, exclude) {
									continue
								}
								count++
								if count == nth {
									out = vals[k]
									break
								}
							}
						}
					}
					results[ordered[pos]] = out
				}
			}
		}
		for i := range rows {
			rows[i] = append(rows[i], results[i])
		}
	}
	return nil
}

// groupWindowSpecs groups window specs that share an identical PARTITION BY + ORDER BY (column
// slots + direction / NULLS / collation; orderSlot is comparable, collations are interned so the
// pointer compares equal), returning the spec indices per group. One partition + per-partition sort
// then serves every spec in a group (window.md §5.2 — the shared partition/sort pass). Grouping is
// stable and the per-spec slot mapping is preserved (each spec still writes its result column in
// spec order), so the optimization is purely a wall-clock win — the cost is unchanged (§8).
func groupWindowSpecs(specs []windowSpec) [][]int {
	var groups [][]int
	for i := range specs {
		placed := false
		for gi := range groups {
			rep := specs[groups[gi][0]]
			if slices.Equal(rep.partition, specs[i].partition) && slices.Equal(rep.order, specs[i].order) {
				groups[gi] = append(groups[gi], i)
				placed = true
				break
			}
		}
		if !placed {
			groups = append(groups, []int{i})
		}
	}
	return groups
}

// cmpRowsByOrder compares two rows by the (all-C) ORDER BY keys — the first non-equal key decides; a
// full tie is 0 (the stable sort then keeps input order). Only used when no key is collated.
func cmpRowsByOrder[R ~[]Value](a, b R, order []orderSlot) int {
	for _, k := range order {
		if c := keyCmp(a[k.idx], b[k.idx], k.descending, k.nullsFirst); c != 0 {
			return c
		}
	}
	return 0
}

// windowCollKeys precomputes each row's collated UCA sort-key bytes for the spec's collated ORDER BY
// slots (if any), indexed in parallel with rows, so the partition sort AND peer determination
// (ranking, frame peer groups) honor the collation identically (window.md §3/§5). Returns nil when no
// key is collated. An unmapped code point fails 0A000 here, at this deterministic per-row point.
func windowCollKeys(rows []storedRow, order []orderSlot) ([][][]byte, error) {
	collated := false
	for _, k := range order {
		if k.collation != nil {
			collated = true
			break
		}
	}
	if !collated {
		return nil, nil
	}
	all := make([][][]byte, len(rows))
	for i, row := range rows {
		var keys [][]byte
		for _, k := range order {
			if k.collation == nil {
				continue
			}
			if row[k.idx].Kind == ValText {
				sk, err := sortKey(k.collation, row[k.idx].str())
				if err != nil {
					return nil, err
				}
				keys = append(keys, sk)
			} else {
				keys = append(keys, nil) // NULL (a collated slot is text) — handled by NULL placement
			}
		}
		all[i] = keys
	}
	return all, nil
}

// cmpWindowRows compares two rows of the window buffer (by their index a/b into the full row slice) by
// the window ORDER BY keys, honoring collation. A collated slot compares the precomputed UCA sort-key
// bytes in collKeys (indexed in parallel with the rows; a nil entry ⇒ a NULL value, NULL placement +
// the descending flip applied here, mirroring cmpDecorated); a non-collated slot compares the row
// values via keyCmp. This one comparator drives the partition sort AND every peer determination
// (ranking, the aggregate default frame, frameCtx's peer groups), so a collated window orders, ranks,
// and frames identically (window.md §3/§5). With no collated key, collKeys is unused and this is
// cmpRowsByOrder by index.
func cmpWindowRows(a, b int, rows []storedRow, order []orderSlot, collKeys [][][]byte) int {
	ci := 0 // advances once per collated slot (keys stored in slot order)
	for _, k := range order {
		var c int
		if k.collation != nil {
			ak, bk := collKeys[a][ci], collKeys[b][ci]
			ci++
			switch {
			case ak == nil && bk == nil:
				c = 0
			case ak == nil:
				if k.nullsFirst {
					c = -1
				} else {
					c = 1
				}
			case bk == nil:
				if k.nullsFirst {
					c = 1
				} else {
					c = -1
				}
			default:
				c = bytes.Compare(ak, bk)
				if k.descending {
					c = -c
				}
			}
		} else {
			c = keyCmp(rows[a][k.idx], rows[b][k.idx], k.descending, k.nullsFirst)
		}
		if c != 0 {
			return c
		}
	}
	return 0
}

// sortRowsCollated sorts rows when at least one ORDER BY key is collated (spec/design/collation.md
// §6/§8). Decorate-sort-undecorate: each collated key's UCA sort key is built ONCE per row up front
// (propagating a SortKey failure — e.g. 0A000 for an unmapped code point — at this deterministic
// per-row point, not inside the comparator), then the rows are sorted by the precomputed key bytes
// for collated slots and the value comparator for the rest. The sort is UNMETERED like every sort
// (cost.md §3); the collate cost is charged at the comparison evaluator (collation.md §11). A
// collated ORDER BY is in-memory only this slice, so this never spills (collated keys are slice 1e).
func sortRowsCollated[R ~[]Value](rows []R, order []orderSlot) error {
	type deco struct {
		keys [][]byte // one per collated slot, in slot order; nil entry ⇒ a NULL value
		row  R
	}
	d := make([]deco, len(rows))
	for i, row := range rows {
		var keys [][]byte
		for _, k := range order {
			if k.collation == nil {
				continue
			}
			if row[k.idx].Kind == ValText {
				sk, err := sortKey(k.collation, row[k.idx].str())
				if err != nil {
					return err
				}
				keys = append(keys, sk)
			} else {
				keys = append(keys, nil) // NULL (a collated slot is text) — handled by NULL placement
			}
		}
		d[i] = deco{keys: keys, row: row}
	}
	sort.SliceStable(d, func(a, b int) bool {
		return cmpDecorated(d[a].keys, d[a].row, d[b].keys, d[b].row, order) < 0
	})
	for i := range d {
		rows[i] = d[i].row
	}
	return nil
}

// cmpDecorated compares two decorated rows (precomputed collated-key bytes + the row) by the ORDER BY
// keys. A collated slot compares its precomputed sort-key bytes (NULL placement + the descending flip
// applied here, mirroring keyCmp); a non-collated slot compares the row values via keyCmp.
func cmpDecorated[R ~[]Value](akeys [][]byte, arow R, bkeys [][]byte, brow R, order []orderSlot) int {
	ci := 0 // advances once per collated slot (keys stored in slot order)
	for _, k := range order {
		var c int
		if k.collation != nil {
			ak, bk := akeys[ci], bkeys[ci]
			ci++
			switch {
			case ak == nil && bk == nil:
				c = 0
			case ak == nil:
				if k.nullsFirst {
					c = -1
				} else {
					c = 1
				}
			case bk == nil:
				if k.nullsFirst {
					c = 1
				} else {
					c = -1
				}
			default:
				c = bytes.Compare(ak, bk)
				if k.descending {
					c = -c
				}
			}
		} else {
			c = keyCmp(arow[k.idx], brow[k.idx], k.descending, k.nullsFirst)
		}
		if c != 0 {
			return c
		}
	}
	return 0
}

// keyCmp is one ORDER BY key's total-order comparison, returning <0, 0, >0. NULL placement
// is governed by nullsFirst and applied INDEPENDENTLY of the value-direction flip
// (descending), so an explicit NULLS FIRST|LAST overrides the direction default
// (spec/design/grammar.md §10). The physical key order ratifies NULL as the largest value
// (the PostgreSQL model), which surfaces as the parse-time default nullsFirst = descending.
func keyCmp(a, b Value, descending, nullsFirst bool) int {
	switch {
	case a.Kind == ValNull && b.Kind == ValNull:
		return 0
	case a.Kind == ValNull:
		if nullsFirst {
			return -1
		}
		return 1
	case b.Kind == ValNull:
		if nullsFirst {
			return 1
		}
		return -1
	}
	base := valueCmp(a, b)
	if descending {
		return -base
	}
	return base
}

// valueCmp is the total order over NON-NULL values: signed-integer ascending, text by
// the C collation — raw UTF-8 bytes, which for UTF-8 equals code-point order (Go's
// strings.Compare is byte order — spec/design/types.md §11) — and boolean by value,
// false < true (orderKey maps false→0, true→1; types.md §9). The cross-family arms are
// defined only for totality — ORDER BY is over a single typed column, so a mixed pair is
// unreachable from SELECT. NULLs are handled by keyCmp before this is reached. Returns
// <0, 0, >0.
func valueCmp(a, b Value) int {
	switch {
	case a.Kind == ValInt && b.Kind == ValInt:
		return cmpInt64(a.Int, b.Int)
	case a.Kind == ValDecimal && b.Kind == ValDecimal:
		return a.decimal().CmpValue(*b.decimal())
	case a.Kind == ValText && b.Kind == ValText:
		return strings.Compare(a.str(), b.str())
	case a.Kind == ValBytea && b.Kind == ValBytea:
		// bytea is held in Str (raw bytes); strings.Compare is unsigned byte order.
		return strings.Compare(a.str(), b.str())
	case a.Kind == ValUuid && b.Kind == ValUuid:
		// uuid's 16 raw bytes are held in Str; strings.Compare is unsigned byte order.
		return strings.Compare(a.str(), b.str())
	case a.Kind == ValBool && b.Kind == ValBool:
		return cmpInt64(newOrderKey(a), newOrderKey(b))
	case a.Kind == ValTimestamp && b.Kind == ValTimestamp:
		return cmpInt64(a.Int, b.Int)
	case a.Kind == ValTimestamptz && b.Kind == ValTimestamptz:
		return cmpInt64(a.Int, b.Int)
	case a.Kind == ValDate && b.Kind == ValDate:
		return cmpInt64(a.Int, b.Int)
	case a.Kind == ValInterval && b.Kind == ValInterval:
		// Intervals order by the canonical 128-bit span (spec/design/interval.md §2).
		return a.interval().SpanCmp(b.interval())
	case a.IsFloat() && b.IsFloat():
		// The PG float8 TOTAL order: -0 = +0, NaN = NaN, NaN largest (spec/design/float.md §3).
		// Mixed widths widen to f64 (lossless). Drives ORDER BY / MIN / MAX / DISTINCT / GROUP BY.
		return floatTotalCmp(a.asF64(), b.asF64())
	case a.Kind == ValComposite && b.Kind == ValComposite:
		// A composite sorts lexicographically, NULLs-last per field (the composite sort key —
		// spec/design/composite.md §5): the first non-equal field decides, recursing through keyCmp
		// so per-field NULL placement and nested composites are handled uniformly. The caller's
		// `descending` flip in keyCmp reverses the whole tuple. A row-size tie-break keeps it total
		// (same-type rows have equal arity, so it is only reached for safety).
		x, y := *a.composite(), *b.composite()
		for i := 0; i < len(x) && i < len(y); i++ {
			if c := keyCmp(x[i], y[i], false, false); c != 0 {
				return c
			}
		}
		return cmpInt64(int64(len(x)), int64(len(y)))
	case a.Kind == ValArray && b.Kind == ValArray:
		// An array sorts by the PG array_cmp total order (spec/design/array.md §5): element-wise over
		// the flattened elements (NULLs-last per element, recursing through keyCmp), then fewer
		// elements first, then smaller ndim, then per dimension (length, then lower bound).
		x, y := a.arrayVal(), b.arrayVal()
		for i := 0; i < len(x.Elements) && i < len(y.Elements); i++ {
			if c := keyCmp(x.Elements[i], y.Elements[i], false, false); c != 0 {
				return c
			}
		}
		if c := cmpInt(len(x.Elements), len(y.Elements)); c != 0 {
			return c
		}
		if c := cmpInt(x.Ndim(), y.Ndim()); c != 0 {
			return c
		}
		for d := 0; d < x.Ndim(); d++ {
			if c := cmpInt(x.Dims[d], y.Dims[d]); c != 0 {
				return c
			}
			if c := cmpInt(int(x.Lbounds[d]), int(y.Lbounds[d])); c != 0 {
				return c
			}
		}
		return 0
	case a.Kind == ValRange && b.Kind == ValRange:
		// A range sorts by the PG range_cmp total order (spec/design/ranges.md §6): `empty` below every
		// non-empty, then lower bound, then upper bound (accounting for infinity/inclusivity). Kept
		// identical to value.Lt3/Gt3's range arm so `<` and ORDER BY never disagree.
		return rangeTotalCmp(a.rangeVal(), b.rangeVal())
	case a.Kind == ValJsonb && b.Kind == ValJsonb:
		// jsonb sorts by PG's total btree order (spec/design/json.md §5); kept identical to
		// value.Lt3/Gt3's jsonb arm so `<` and ORDER BY never disagree. (json never sorts — the
		// resolver rejects it 42883.)
		return a.jsonb().Cmp(b.jsonb())
	default:
		// Cross-family arms exist only for totality — ORDER BY is over a single typed column,
		// so a mixed pair is unreachable. A fixed family order keeps the comparator total.
		return cmpInt64(int64(familyRank(a)), int64(familyRank(b)))
	}
}

func cmpInt64(x, y int64) int {
	switch {
	case x < y:
		return -1
	case x > y:
		return 1
	default:
		return 0
	}
}

func newOrderKey(v Value) int64 {
	if v.Kind == ValBool {
		if v.boolVal() {
			return 1
		}
		return 0
	}
	return v.Int
}

// familyRank is a fixed total order across value families, for the unreachable cross-family
// case of valueCmp (ORDER BY is single-column-typed).
func familyRank(v Value) int {
	switch v.Kind {
	case ValNull:
		return 0
	case ValBool:
		return 1
	case ValInt:
		return 2
	case ValDecimal:
		return 3
	case ValText:
		return 4
	case ValBytea:
		return 5
	case ValUuid:
		return 6
	case ValTimestamp:
		return 7
	case ValTimestamptz:
		return 8
	case ValInterval:
		return 9
	case ValFloat32:
		return 10
	case ValFloat64:
		return 11
	case ValDate:
		return 13
	case ValJson:
		// json never sorts (42883 at resolve); jsonb sorts only against jsonb. Cross-family ranks for
		// totality only — they sit after the scalar/container families.
		return 15
	case ValJsonb:
		return 16
	case ValJsonPath:
		// jsonpath never sorts (42883 at resolve); a cross-family rank for totality only.
		return 17
	default:
		return 12
	}
}
