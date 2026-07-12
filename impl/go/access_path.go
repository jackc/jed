package jed

import (
	"bytes"
	"slices"
	"sort"
	"strings"
)

// Access-path MECHANISMS — turning a WHERE filter into a bounded scan (spec/design/cost.md §3;
// planner.md §4 — the machinery the optimize.go rules call, not the rules themselves: it also
// serves UPDATE/DELETE planning and exec-time eligibility). This file holds the row-source scan
// interface (rowSource/scanSource), the access-plan shapes
// (pkBoundPlan/scanBound/indexBoundPlan/gistBoundPlan/ginBoundPlan/keySet plans), the predicate
// analysis that detects a point lookup / range / index / GIN / GiST bound from a filter
// (detectPKBound/detectScanBound/detectKeySet/detectGinBound/detectGistBound, buildIndexAccessPredicate),
// the ORDER-BY-via-scan-order analysis (orderSatisfiedByPK/orderSatisfiedByIndex),
// the order-preserving key-bound encoding (buildKeyBound/encodeBoundKey/encodeTextBound), and the
// streaming/window-top-N eligibility checks.

// rowSource is a pull-based row cursor (Volcano-style): each next() yields one row, or
// (nil, false, nil) at end of stream. The evaluation environment and the cost meter are
// threaded IN per call rather than stored as fields, so a source owns no borrow and the one
// meter is charged down a single call path with no aliasing (the discipline that keeps the
// Rust mirror free of lifetime entanglement — CLAUDE.md §2). This is the seam the streaming +
// point-lookup work (TODO Phase 6) builds on; today only scanSource exists and feeds the
// existing materialize-then-join pipeline unchanged, so results and cost are byte-identical.
type rowSource interface {
	next(env *evalEnv, m *costMeter) (storedRow, bool, error)
}

// scanSource streams a base table's rows in primary-key order. It charges the page_read block
// (one per B-tree node — spec/design/cost.md §3 "page_read") once, before the first row, then
// storage_row_read per row yielded: the same units in the same order as the inline scan loop it
// replaced. rows is the in-key-order materialization (eager today, via IterInKeyOrder; a lazy
// leaf walk later) — the charge accounting is identical either way because cost is the logical
// node/row count, not a physical leaf fetch (pager.md §5). The block fires on the first next()
// even for an empty table (nodeCount 0 ⇒ a no-op charge), so the accrued total never moves.
type scanSource struct {
	rows         []storedRow
	i            int
	nodeCount    int
	chargedBlock bool
}

func (s *scanSource) next(env *evalEnv, m *costMeter) (storedRow, bool, error) {
	// Enforce the cost ceiling before pulling the next row (CLAUDE.md §13): a runaway scan (or a
	// JOIN/correlated re-scan built on this source) stops deterministically once accrued cost
	// reaches the limit. No-op when unlimited (spec/design/cost.md §6).
	if err := m.Guard(); err != nil {
		return nil, false, err
	}
	if !s.chargedBlock {
		m.Charge(costs.PageRead * int64(s.nodeCount))
		s.chargedBlock = true
	}
	if s.i >= len(s.rows) {
		return nil, false, nil
	}
	m.Charge(costs.StorageRowRead)
	row := s.rows[s.i]
	s.i++
	return row, true, nil
}

// ---- Primary-key predicate pushdown (spec/design/cost.md §3 "bounded scan / point lookup") ----
//
// A single-table WHERE on the primary key bounds the storage-key range a scan must visit. Detection
// is two-stage: detectPKBound runs at plan time (structural — which conjuncts are PK comparisons),
// buildKeyBound at exec time (the const values, and any $N, are known only then). The bound is a
// SUPERSET of the matching keys: the whole WHERE stays the residual filter (re-applied to each
// scanned row), so the result is always correct — the bound only narrows which rows are scanned, and
// the page_read/storage_row_read drop to what it touches. The unbounded case (nil pkBound) keeps the
// full scan, so its cost never moves.

// boundTerm is one resolved `pk <op> const-source` from a WHERE AND-chain, normalized so the PK is
// the LEFT side (a `5 < pk` flips to `pk > 5`). src is the constant/parameter operand.
type boundTerm struct {
	op  binaryOp
	src *rExpr
}

// pkBoundPlan is the plan-time result of PK analysis: the PK column's storage type + the bound
// terms. The concrete key range is built per execution by buildKeyBound.
type pkBoundPlan struct {
	pkType scalarType
	terms  []boundTerm
	// coll is the key column's resolved collation when it is collated AND Full (loaded version
	// matches the file pin) — the probe encodes via this collation's UCA sort key (encoding.md
	// §2.12), seeking the same key FORM the B-tree stores (spec/design/collation.md §8). nil for a
	// C (raw-byte) key. A Skewed collated key never produces a pkBoundPlan (keyCollationCtx refuses
	// the bound — collation.md §12).
	coll *Collation
}

// scanBound is a per-relation scan bound (cost.md §3): a primary-key range, a
// secondary-index equality (spec/design/indexes.md §5), a GIN-bounded scan over an
// array column (spec/design/gin.md §6), a GiST-bounded scan, or a MERGED point-set (an
// OR / IN-list of key equalities lowered to a union of point probes — cost.md §3 "OR /
// IN-list") — exactly one field is set. The PK bound wins when several apply (it is the
// row's own key — no second tree, range-capable, strictly cheaper); the ordered-index
// equality bound wins over GIN (gin.md §6). The point-set bounds (pkSet/indexSet) are a
// LAST-RESORT access path, chosen only when no contiguous PK/index/GIN/GiST bound applies,
// so they never displace an existing plan.
type scanBound struct {
	pk       *pkBoundPlan
	index    *indexBoundPlan
	gin      *ginBoundPlan
	gist     *gistBoundPlan
	pkSet    *pkKeySetPlan
	indexSet *indexKeySetPlan
}

// mutationScanPlan is the small physical plan shared by UPDATE/DELETE execution and DML EXPLAIN.
// The resolved filter remains the residual predicate; bound is only the chosen candidate superset.
// scope carries the target database qualifier so a full scan continues through the scoped store
// funnel (attachments deliberately have no bound this slice).
type mutationScanPlan struct {
	bound  *scanBound
	filter *rExpr
	scope  *string
}

// mutationScanBatch is the normalized result of executing any mutation access path. Every path
// returns storage keys with rows (SELECT may discard keys through its row wrappers), plus the exact
// up-front page/decompression units its caller charges before per-row storage_row_read.
type mutationScanBatch struct {
	entries []entry
	pages   int
	slabs   int
	empty   bool
}

// needsEagerScan reports whether a bound needs the general eager materialize path (materializeRel /
// the DML scan) rather than a single-contiguous-range fast path (streaming scan, columnar project,
// vectorized aggregate, streaming sort, join top-N). True for a second-tree gather (index / GIN /
// GiST) and for a merged OR/IN point-set (pkSet / indexSet — a union of probes, cost.md §3
// "OR / IN-list"); false for a nil bound or a plain PK contiguous bound (which every fast path
// handles via a single buildKeyBound). Every single-table fast-path gate consults this so the
// point-set bounds are interpreted in exactly ONE place (materializeRel), never silently dropped to
// a full scan by a fast path that only understands `pk`. Nil-safe (a nil bound is not eager).
func (sb *scanBound) needsEagerScan() bool {
	return sb != nil && (sb.index != nil || sb.gin != nil || sb.gist != nil || sb.pkSet != nil || sb.indexSet != nil)
}

// pkKeySetPlan is the plan-time result of an OR / IN-list disjunction of primary-key
// equalities (`pk = a OR pk = b OR …`, or the equivalent `pk IN (a, b, …)` which desugars
// to that OR-chain — cost.md §3 "OR / IN-list"). srcs is the equality const-sources, one
// per disjunct, in source order (a bind param, an outer/correlated column, or a literal —
// isConstSource of the PK type). At exec time each src encodes into the PK key space; the
// resulting keys are de-duplicated and sorted, and each becomes a point probe [k, k]. The
// whole WHERE stays the residual filter (the union is a superset), so the result is
// unchanged. coll is the PK's key collation (nil for a C key), as in pkBoundPlan.
type pkKeySetPlan struct {
	pkType scalarType
	coll   *Collation
	srcs   []*rExpr
}

// indexKeySetPlan is the pkKeySetPlan analog over a leading B-tree secondary-index column
// (indexes.md §5): each distinct encoded value becomes an index point probe (prefix scan +
// per-entry row lookup), and the rows are gathered in ascending value order. tailTypes is
// the remaining key components' types (as in indexBoundPlan) — the per-entry key-suffix
// skip.
type indexKeySetPlan struct {
	nameKey   string
	colType   scalarType
	coll      *Collation
	tailTypes []scalarType
	srcs      []*rExpr
}

// gistBoundPlan is the plan-time result of GiST analysis (spec/design/gist.md §5): the chosen GiST
// index (lowest lowercased name whose range column has a `col && const` / `col @> const` conjunct),
// the descent strategy, and the column's global scope index. Like ginBoundPlan, the constant query
// operand is NOT stored (re-found in plan.filter at exec time by gistMatch). No element type is
// carried — the gather descends the resident R-tree (gist.md §4.1), whose bounds are already decoded.
type gistBoundPlan struct {
	nameKey   string
	strategy  gistStrategy
	colGlobal int
	// scalarType is the GiST-indexed column's scalar type for the scalar `=` opclass (strategy
	// gistEqual, GX2): gistBoundRows encodes the equality constant to its order-preserving key bytes
	// with it. Unused for range_ops, whose &&/@> query is a range constant the R-tree compares directly.
	scalarType scalarType
}

// ginStrategy is which array operator a GIN bound accelerates (spec/design/gin.md §6): @>
// (contains, mode ALL → posting-list intersection), && (overlaps, mode ANY → union), = ANY
// (member — `c = ANY(col)`, the single-term @> reduction: one scalar term, its lone posting list),
// or array = (equal — `col = Q`, the @>-superset gather + residual =).
type ginStrategy int

const (
	ginContains ginStrategy = iota
	ginOverlaps
	// ginMember is `c = ANY(col)`: c is a constant SCALAR (not an array); its single term is
	// gathered like a one-element @>. The query operand recovered by ginMatch is the scalar c.
	ginMember
	// ginEqual is `col = Q`: exact array equality. The query operand is the constant array Q; its
	// distinct non-NULL elements gather the SAME candidate superset as `@> Q` (equal arrays have
	// identical element multisets, so col = Q ⟹ col @> Q), and the residual = filter makes it
	// exact. Unlike ginContains, a NULL ELEMENT of Q does not empty the bound; and a Q with no
	// non-NULL element ('{}'/all-NULL) falls back to the full scan, not a provably-empty bound.
	ginEqual
)

// ginBoundPlan is the plan-time result of GIN analysis (spec/design/gin.md §6): the chosen
// GIN index (lowest lowercased name whose array column has a `col @> const` / `col && const`
// conjunct), the array ELEMENT type (for encode(term) — the term bytes), the operator
// strategy, and the column's global scope index. The constant query Q is NOT stored; it is
// re-found in plan.filter at exec time by ginMatch and evaluated there.
type ginBoundPlan struct {
	nameKey   string
	elemType  scalarType
	strategy  ginStrategy
	colGlobal int
}

// indexEqCol is one column of an index access predicate's equality prefix (indexes.md §5.1):
// the column's storage type, its key collation (nil unless it is a Full-collated text column),
// and every equality const-source bound to it. At exec time the sources must agree on one value
// (else the bound is provably empty). A collated column encodes its probe via the UCA sort key
// (encoding.md §2.12) to match the index's stored key form (collation.md §8).
type indexEqCol struct {
	colType scalarType
	coll    *Collation
	srcs    []*rExpr
}

// indexBoundPlan is the plan-time result of index analysis (indexes.md §5.1): the chosen index
// (lowest lowercased name yielding a non-empty access predicate) and the predicate — a maximal
// EQUALITY PREFIX on the leading key columns (eqCols) plus an OPTIONAL RANGE on the next column
// (rangeTerms / rangeType). At exec time buildIndexBound turns these into a concrete index-key
// range: the equality prefix bytes P = concatenated present slots, then the range (if any)
// intersected relative to P. suffixTypes are the types of the index columns AFTER the equality
// prefix (columns[len(eqCols):]) — the range column (if any) plus every trailing column — each
// FIXED-WIDTH so an admitted entry's row-key suffix is recovered by width-skipping them past P.
type indexBoundPlan struct {
	nameKey     string // the index store's key — the lowercased index name
	eqCols      []indexEqCol
	rangeType   scalarType  // the range column's type (meaningful iff rangeTerms != nil)
	rangeTerms  []boundTerm // range conjuncts on the column after the equality prefix (nil ⇒ none)
	suffixTypes []scalarType
}

// buildIndexAccessPredicate constructs an index access predicate for idx over rel (indexes.md
// §5.1): a maximal EQUALITY PREFIX on the leading key columns plus an OPTIONAL RANGE on the next
// column. It walks the index's key columns in key order against the WHERE AND-chain, consuming a
// column with an agreed equality conjunct into the prefix and stopping at the first column that
// has no equality (taking its range conjuncts, if any, as the trailing range). Returns nil for a
// non-B-tree index, a Skewed collated bound column (whose stored keys are at the file's pinned
// version — collation.md §12), no bound at all, or an ineligible suffix (a column after the
// equality prefix that is not a fixed-width scalar — the width-based key-suffix skip needs it).
// siblingCutoff opens the index-nested-loop door (>= 0 admits a bare sibling reColumn as a bound
// source, resolved per outer row); -1 is the ordinary once-materialized bound.
func (db *engine) buildIndexAccessPredicate(filter *rExpr, rel scopeRel, idx indexDef, siblingCutoff int) *indexBoundPlan {
	if idx.Kind != indexBtree {
		return nil
	}
	// Resolve the index's key elements (column ordinals + resolved expression keys). A resolution
	// failure yields no bound (a full scan — always sound). indexes.md §5.
	rindex, err := db.resolveIndex(rel.table, idx)
	if err != nil {
		return nil
	}
	// A PARTIAL index holds only its qualifying rows (indexes.md §9), so it is usable ONLY when the
	// query's WHERE implies the index predicate. jed's test is syntactic (PG's, not a prover): the
	// WHERE AND-chain must contain a conjunct STRUCTURALLY EQUAL to the resolved predicate. A miss
	// yields no bound — a correct full scan. (The resolved predicate is in table-local column coords;
	// a WHERE conjunct is global, so it is matched shifted by rel.offset.)
	if rindex.Predicate != nil && !filterImpliesPredicate(filter, rindex.Predicate, rel.offset) {
		return nil
	}
	var eqCols []indexEqCol
	var rangeType scalarType
	var rangeTerms []boundTerm
	for i := range rindex.Keys {
		key := rindex.Keys[i]
		// Each key element yields (its scalar key type, its key collation, the matcher against a
		// WHERE conjunct operand). A non-scalar / skewed element stops the prefix.
		var ty scalarType
		var coll *Collation
		var matcher keyMatch
		if key.Expr == nil {
			ci := key.Col
			s, ok := rel.table.Columns[ci].Type.AsScalar()
			if !ok {
				break // a range/array/composite column cannot be seeked
			}
			// Collation.md §8/§12: a Skewed collated column refuses the bound (its stored keys are
			// wrong for the loaded bundle) — stop the prefix. C/Full admissible.
			c, push := db.keyCollationCtx(rel.table.Columns[ci])
			if !push {
				break
			}
			ty, coll, matcher = s, c, columnMatch(rel.offset+ci)
		} else {
			// An expression key seeks only when its result is a scalar and its collation is C (the
			// common lower(email) shape). A collated-expression bound is a deferred follow-on (§5).
			// Match a WHERE operand structurally against the key.
			s, ok := key.Ty.AsScalar()
			if !ok {
				break
			}
			if key.Coll != nil {
				break
			}
			ty, coll, matcher = s, nil, exprMatch(key.Expr, rel.offset)
		}
		colColl := ""
		if coll != nil {
			colColl = coll.Name
		}
		var eqs []*rExpr
		var ranges []boundTerm
		var walk func(e *rExpr)
		walk = func(e *rExpr) {
			if e == nil {
				return
			}
			if e.kind == reAnd {
				walk(e.lhs)
				walk(e.rhs)
				return
			}
			if t, ok := asBoundTerm(e, matcher, ty, colColl, siblingCutoff); ok {
				if t.op == opEq {
					eqs = append(eqs, t.src)
				} else {
					ranges = append(ranges, t)
				}
			}
		}
		walk(filter)
		if len(eqs) > 0 {
			eqCols = append(eqCols, indexEqCol{colType: ty, coll: coll, srcs: eqs})
			continue // extend the equality prefix
		}
		if len(ranges) > 0 {
			rangeType = ty
			rangeTerms = ranges
		}
		break // first non-equality element ends the prefix (with or without a trailing range)
	}
	if len(eqCols) == 0 && rangeTerms == nil {
		return nil // nothing bound
	}
	// Eligibility: every key element from the range element onward (keys[len(eqCols):]) is
	// width-skipped past the known equality prefix to recover the storage key, so each must be a
	// fixed-width scalar (a column's type, or an expression's result type). The equality-prefix
	// elements may be any width — their slots are matched as the known prefix bytes.
	suffix := make([]scalarType, 0, len(rindex.Keys)-len(eqCols))
	for _, key := range rindex.Keys[len(eqCols):] {
		var s scalarType
		var ok bool
		if key.Expr == nil {
			s, ok = rel.table.Columns[key.Col].Type.AsScalar()
		} else {
			s, ok = key.Ty.AsScalar()
		}
		if !ok || !s.IsFixedWidth() {
			return nil
		}
		suffix = append(suffix, s)
	}
	return &indexBoundPlan{
		nameKey: strings.ToLower(idx.Name), eqCols: eqCols,
		rangeType: rangeType, rangeTerms: rangeTerms, suffixTypes: suffix,
	}
}

// scanBoundPolicy is the consumer-specific eligibility/precedence part of access-path selection.
// SELECT and mutation scans share one inventory below. Their candidate sets now differ only in the
// established GiST/GIN precedence: mutations try GIN before GiST while SELECT tries GiST before GIN.
// Keeping that difference as data here prevents EXPLAIN and execution from growing separate ladders.
type scanBoundPolicy struct {
	orderedIndex  bool
	indexSet      bool
	gistBeforeGin bool
}

var (
	selectScanBoundPolicy   = scanBoundPolicy{orderedIndex: true, indexSet: true, gistBeforeGin: true}
	mutationScanBoundPolicy = scanBoundPolicy{orderedIndex: true, indexSet: true}
)

// detectScanBound picks one SELECT relation's scan bound (cost.md §3; indexes.md §5). It is the
// SELECT-policy wrapper over the shared inventory in detectScanBoundWithPolicy.
func detectScanBound(filter *rExpr, rel scopeRel, db *engine) *scanBound {
	return detectScanBoundWithPolicy(filter, rel, db, selectScanBoundPolicy)
}

// detectScanBoundWithPolicy inventories the structurally usable bounds for one base relation and
// returns the first candidate admitted by policy. The whole WHERE remains the residual filter, so a
// disabled candidate always falls through to another sound bound or a full scan.
func detectScanBoundWithPolicy(filter *rExpr, rel scopeRel, db *engine, policy scanBoundPolicy) *scanBound {
	// A host-attached relation full-scans this slice (attached-databases.md §8): the bounded-scan exec
	// path resolves index stores unscoped, so no PK/index/GiST/GIN bound may apply to an attachment.
	if rel.isAttachment() {
		return nil
	}
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		// Ordered-equality pushdown is scalar-only; a non-scalar (range) PK skips it (point-lookup
		// deferred for containers — ranges.md §10), falling through to a full scan + residual filter.
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			// The PK column's key collation form (collation.md §8/§12): push=false ⇒ collated but
			// Skewed ⇒ refuse pushdown (full heap-scan recompute — the read-safety rule §12); else
			// coll is nil (C, raw-byte key) or the Full-collated table (push via the sort key).
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				if bp := detectPKBound(filter, rel.offset+pkLocal, sty, coll); bp != nil {
					return &scanBound{pk: bp}
				}
			}
		}
	}
	if policy.orderedIndex {
		for _, idx := range rel.table.Indexes {
			// An index access predicate (indexes.md §5.1): a maximal equality prefix + optional trailing
			// range over a B-tree index's leading key columns. Returns nil for a GIN/GiST index (handled
			// by the passes below), an ineligible tail, or no bound. Indexes are held in ascending
			// lowercased-name order, so the first non-nil wins — the deterministic tie-break.
			if ib := db.buildIndexAccessPredicate(filter, rel, idx, -1); ib != nil {
				return &scanBound{index: ib}
			}
		}
	}
	if policy.gistBeforeGin {
		// SELECT's established order is GiST then GIN after PK/ordered B-tree.
		if gb := detectGistBound(filter, rel.table.Indexes, rel.table.Columns, rel.offset); gb != nil {
			return &scanBound{gist: gb}
		}
		if gb := detectGinBound(filter, rel.table.Indexes, rel.table.Columns, rel.offset); gb != nil {
			return &scanBound{gin: gb}
		}
	} else {
		// UPDATE/DELETE's established order is GIN then GiST after PK. Phase 0 preserves it.
		if gb := detectGinBound(filter, rel.table.Indexes, rel.table.Columns, rel.offset); gb != nil {
			return &scanBound{gin: gb}
		}
		if gb := detectGistBound(filter, rel.table.Indexes, rel.table.Columns, rel.offset); gb != nil {
			return &scanBound{gist: gb}
		}
	}
	// LAST RESORT — an OR / IN-list of key equalities lowered to merged point probes
	// (cost.md §3 "OR / IN-list"). Reached only when no contiguous PK/index/GIN/GiST bound
	// applied above, so this never displaces an existing plan. The primary key wins over a
	// secondary index (its own key — no second tree), matching detectScanBound's ordering.
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				if srcs := detectKeySet(filter, rel.offset+pkLocal, sty, coll); srcs != nil {
					return &scanBound{pkSet: &pkKeySetPlan{pkType: sty, coll: coll, srcs: srcs}}
				}
			}
		}
	}
	if !policy.indexSet {
		return nil
	}
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexBtree {
			continue
		}
		// A PARTIAL index is not used for the OR/IN point-set this slice (indexes.md §9): the merged
		// point-lookup path carries no predicate-implication gate, so it stays non-partial (a
		// follow-on) — falling through leaves a correct full scan.
		if idx.Predicate != nil {
			continue
		}
		// The OR/IN merged-point-lookup bound is column-only this slice (an expression index takes
		// the access-predicate path — indexes.md §5; OR/IN over an expression key is a follow-on).
		cols := idx.columnOrdinals()
		if cols == nil {
			continue
		}
		ci := cols[0]
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			continue
		}
		unskippableTail := false
		for _, c := range cols[1:] {
			s, ok := rel.table.Columns[c].Type.AsScalar()
			if !ok || !s.IsFixedWidth() {
				unskippableTail = true
				break
			}
		}
		if unskippableTail {
			continue
		}
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			continue
		}
		if srcs := detectKeySet(filter, rel.offset+ci, ty, coll); srcs != nil {
			tail := make([]scalarType, 0, len(cols)-1)
			for _, c := range cols[1:] {
				tail = append(tail, rel.table.Columns[c].Type.ScalarTy())
			}
			return &scanBound{indexSet: &indexKeySetPlan{
				nameKey: strings.ToLower(idx.Name), colType: ty, coll: coll, tailTypes: tail, srcs: srcs,
			}}
		}
	}
	return nil
}

// planMutationScan selects an UPDATE/DELETE target access path through the same inventory as SELECT,
// using the mutation eligibility policy. It runs after uncorrelated filter folding, matching the old
// inline executor timing. EXPLAIN calls the same function on its resolved (unfolded) filter.
func (db *engine) planMutationScan(scope *string, table *catTable, filter *rExpr) mutationScanPlan {
	plan := mutationScanPlan{filter: filter, scope: scope}
	if filter == nil {
		return plan
	}
	rel := scopeRel{label: strings.ToLower(table.Name), table: table, offset: 0, db: scope}
	plan.bound = detectScanBoundWithPolicy(filter, rel, db, mutationScanBoundPolicy)
	return plan
}

// detectKeySet finds an OR / IN-list disjunction of equalities on ONE key column (at global
// index keyIdx) and returns its equality const-sources (one per disjunct), or nil if the
// filter has no such shape (cost.md §3 "OR / IN-list"). `x IN (a, b, c)` desugars to
// `x = a OR x = b OR x = c` at resolve time (grammar.md §20), so an IN-list and a hand-written
// OR-of-equalities present the identical tree — this handles both. The filter's top-level
// AND-chain is flattened; the FIRST conjunct that reduces to a pure disjunction of
// `keycol = const` equalities is used (the rest of the WHERE stays the residual filter). A
// conjunct reduces iff it is a `keycol = const`, or an OR of two reducing operands — an AND,
// a NOT, a range comparison, or an equality on a different column makes it non-reducing, so a
// mixed disjunction (`pk = 1 OR x = 2`) or a NOT IN (`NOT (pk = 1 OR …)`) correctly yields
// no bound. Conservative + sound: an unrecognized filter contributes no bound.
func detectKeySet(filter *rExpr, keyIdx int, keyType scalarType, coll *Collation) []*rExpr {
	if filter == nil {
		return nil
	}
	colColl := ""
	if coll != nil {
		colColl = coll.Name
	}
	var found []*rExpr
	var walkAnd func(e *rExpr)
	walkAnd = func(e *rExpr) {
		if found != nil || e == nil {
			return
		}
		if e.kind == reAnd {
			walkAnd(e.lhs)
			walkAnd(e.rhs)
			return
		}
		if srcs, ok := reduceKeySet(e, keyIdx, keyType, colColl); ok {
			found = srcs
		}
	}
	walkAnd(filter)
	return found
}

// reduceKeySet reduces one predicate to the set of equality const-sources it bounds keyIdx
// with, or (nil, false) if it is not a pure disjunction of `keycol = const` (detectKeySet).
// Descends OR nodes only; a single `keycol = const` leaf is the base case (reusing
// asBoundTerm, siblingCutoff -1 — no sibling references in a once-materialized bound).
func reduceKeySet(e *rExpr, keyIdx int, keyType scalarType, colColl string) ([]*rExpr, bool) {
	if e == nil {
		return nil, false
	}
	if e.kind == reOr {
		l, lok := reduceKeySet(e.lhs, keyIdx, keyType, colColl)
		if !lok {
			return nil, false
		}
		r, rok := reduceKeySet(e.rhs, keyIdx, keyType, colColl)
		if !rok {
			return nil, false
		}
		return append(l, r...), true
	}
	if t, ok := asBoundTerm(e, columnMatch(keyIdx), keyType, colColl, -1); ok && t.op == opEq {
		return []*rExpr{t.src}, true
	}
	return nil, false
}

// detectINLBound detects an index-nested-loop scan bound for a join inner relation rel (cost.md §3
// "JOIN"): a primary-key (or leading secondary-index column) comparison to a SIBLING column of an
// EARLIER join relation, taken from the join's `on` predicate OR the `whereFilter`. Unlike
// detectScanBound (constants only), this admits a bare sibling column (a reColumn whose global index
// is < rel.offset), resolved per outer row from the current combined left-hand row — the join analog
// of a correlated subquery's outer reference (query.correlated_pushdown). So the inner relation seeks
// per outer row instead of full-scanning for every outer row: O(N·M) → O(N·log M).
//
// Returns non-nil only when the resulting bound has >= 1 sibling term (a reColumn src); a
// constant-only bound is the ordinary once-materialized relBounds path. Constant terms on the same
// key ride along and tighten the per-outer-row seek. The whole on/where stays the residual filter (a
// superset), so the ROWS are unchanged; only the inner re-scan cost drops. Caller restricts this to
// a base table that is the right/nullable side of an INNER/CROSS/LEFT join.
func detectINLBound(on *rExpr, whereFilter *rExpr, rel scopeRel, db *engine) *scanBound {
	// A host-attached inner relation full-scans per outer row this slice (attached-databases.md §8):
	// the seek would resolve its index store unscoped. Index-nested-loop over an attachment is a
	// perf follow-on.
	if rel.isAttachment() {
		return nil
	}
	cutoff := rel.offset
	collect := func(keyIdx int, ty scalarType, coll *Collation) []boundTerm {
		colColl := ""
		if coll != nil {
			colColl = coll.Name
		}
		var terms []boundTerm
		var walk func(e *rExpr)
		walk = func(e *rExpr) {
			if e == nil {
				return
			}
			if e.kind == reAnd {
				walk(e.lhs)
				walk(e.rhs)
				return
			}
			if t, ok := asBoundTerm(e, columnMatch(keyIdx), ty, colColl, cutoff); ok {
				terms = append(terms, t)
			}
		}
		walk(on)
		walk(whereFilter)
		return terms
	}
	hasSibling := func(terms []boundTerm) bool {
		for _, t := range terms {
			if t.src.kind == reColumn {
				return true
			}
		}
		return false
	}
	// Primary-key bound first (the row's own key — range-capable, strictly cheaper).
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				terms := collect(rel.offset+pkLocal, sty, coll)
				if hasSibling(terms) {
					return &scanBound{pk: &pkBoundPlan{pkType: sty, terms: terms, coll: coll}}
				}
			}
		}
	}
	// Else a leading secondary-index equality bound to a sibling (indexes in ascending lowercased
	// name order — the deterministic tie-break, matching detectScanBound).
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexBtree {
			continue
		}
		// The index-nested-loop sibling bound is column-only this slice (an expression index takes
		// the access-predicate path — indexes.md §5; an INL bound over an expression key is a follow-on).
		cols := idx.columnOrdinals()
		if cols == nil {
			continue
		}
		ci := cols[0]
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			continue
		}
		unskippableTail := false
		for _, c := range cols[1:] {
			s, ok := rel.table.Columns[c].Type.AsScalar()
			if !ok || !s.IsFixedWidth() {
				unskippableTail = true
				break
			}
		}
		if unskippableTail {
			continue
		}
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			continue
		}
		terms := collect(rel.offset+ci, ty, coll)
		var eqs []*rExpr
		siblingEq := false
		for _, t := range terms {
			if t.op == opEq {
				eqs = append(eqs, t.src)
				if t.src.kind == reColumn {
					siblingEq = true
				}
			}
		}
		if siblingEq {
			// This slice keeps the index-nested-loop bound single-column-equality (a leading key
			// column bound to a sibling); a multi-column / range INL bound is a follow-on (cost.md
			// §3 "index-nested-loop"). suffixTypes are the trailing columns (columns[1:], fixed-width
			// by the unskippableTail check above), width-skipped past the single equality slot.
			tail := make([]scalarType, 0, len(cols)-1)
			for _, c := range cols[1:] {
				tail = append(tail, rel.table.Columns[c].Type.ScalarTy())
			}
			return &scanBound{index: &indexBoundPlan{
				nameKey:     strings.ToLower(idx.Name),
				eqCols:      []indexEqCol{{colType: ty, coll: coll, srcs: eqs}},
				suffixTypes: tail,
			}}
		}
	}
	return nil
}

// keyCollationCtx reports the collation a key over col is STORED under, deciding whether — and how —
// a comparison bound may push down to that key (spec/design/collation.md §8/§12). Three outcomes:
//   - (nil, true)  — col is C (or non-text): the key is raw bytes (encoding.md §2.4), always
//     pushable, the unchanged fast path.
//   - (coll, true) — col is collated and the collation is Full (its file pin matches the loaded
//     bundle): the key is the UCA sort key (encoding.md §2.12), pushable using coll to encode the
//     probe in the same form.
//   - (nil, false) — col is collated but Skewed (its file pin differs from the loaded bundle): push
//     is REFUSED. The scan stays a full heap-scan that recomputes against the LOADED table (the
//     read-safety rule §12; seeking a loaded-version probe in a file-version B-tree would mis-match —
//     the tripwire suites/collation/skew.test stays green only because this refuses). An
//     unresolvable collation likewise refuses rather than mis-encoding.
func (db *engine) keyCollationCtx(col catColumn) (*Collation, bool) {
	if col.Collation == "" {
		return nil, true
	}
	snap := db.readSnap()
	if _, _, _, _, skewed := snap.collationSkew(col.Collation); skewed {
		return nil, false
	}
	if c := snap.resolveCollation(col.Collation); c != nil {
		return c, true
	}
	return nil, false
}

// detectGinBound detects a GIN-bounded scan over columns/indexes (gin.md §6): the lowest-named
// GIN index whose array column at offset+ci has a GIN-accelerable conjunct (`col @> const`,
// `col && const`, `const = ANY(col)`, or `col = const`). Factored out so the SELECT planner
// (detectScanBound) and the UPDATE/DELETE scan both use the identical detection — the mutations
// pass their own table's indexes/columns at offset 0.
func detectGinBound(filter *rExpr, indexes []indexDef, columns []catColumn, offset int) *ginBoundPlan {
	if filter == nil {
		return nil
	}
	for _, idx := range indexes {
		if idx.Kind != indexGin {
			continue
		}
		ci := idx.firstColumn()
		colGlobal := offset + ci
		at := columns[ci].Type
		if at.Array == nil {
			continue // a GIN column is always an array (the CREATE INDEX gate); defensive
		}
		if s, _, ok := ginMatch(filter, colGlobal); ok {
			return &ginBoundPlan{
				nameKey: strings.ToLower(idx.Name), elemType: at.Array.ScalarTy(), strategy: s, colGlobal: colGlobal,
			}
		}
	}
	return nil
}

// ginMatch finds the first WHERE AND-chain conjunct a GIN index on colGlobal accelerates
// (spec/design/gin.md §6): `col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)`
// (membership), or `col = Q` (exact array equality) where the query operand is a constant
// (references no column / outer / subquery). @> is asymmetric (the indexed column must be the LEFT
// operand — `Q @> col` is the non-accelerated <@); && and array = are symmetric; = ANY requires the
// column be ANY's array operand and c the scalar. Returns the strategy and the constant query
// operand (the scalar c for ginMember, the array Q otherwise). Used at plan time (strategy) and exec
// time (recover the operand from plan.filter), so the two agree on the same conjunct by construction.
func ginMatch(filter *rExpr, colGlobal int) (ginStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := ginMatch(filter.lhs, colGlobal); ok {
			return s, q, true
		}
		return ginMatch(filter.rhs, colGlobal)
	}
	if filter.kind == reArrayFunc && len(filter.sargs) == 2 {
		a, b := filter.sargs[0], filter.sargs[1]
		switch filter.afunc {
		case afContains:
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return ginContains, b, true
			}
		case afOverlaps:
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return ginOverlaps, b, true
			}
			if isColumn(b, colGlobal) && rexprIsConstant(a) {
				return ginOverlaps, a, true
			}
		}
	}
	// `col = Q` — exact array equality (gin.md §6). Commutative: the column may be either operand,
	// the constant array Q the other. Recovered operand is Q; ginBoundRows reads it via ginEqual
	// (the @>-superset gather + the residual =). <> is NOT matched (only OpEq). When the column is an
	// array, the other constant operand is necessarily an array too (resolve rejects array/scalar =).
	if filter.kind == reCompare && filter.op == opEq {
		if isColumn(filter.lhs, colGlobal) && rexprIsConstant(filter.rhs) {
			return ginEqual, filter.rhs, true
		}
		if isColumn(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs) {
			return ginEqual, filter.lhs, true
		}
	}
	// `c = ANY(col)` — the array spelling of membership (gin.md §6): the GIN column must be ANY's
	// ARRAY operand (rhs) and c (the scalar lhs) a constant. Only = ANY (not = ALL, not any other
	// comparison/quantifier — those are not a single-term posting gather). The recovered query
	// operand is the scalar c; ginBoundRows reads it via ginMember.
	if filter.kind == reQuantified && filter.op == opEq && !filter.quantAll &&
		isColumn(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs) {
		return ginMember, filter.lhs, true
	}
	return 0, nil, false
}

// detectGistBound detects a GiST-bounded scan over columns/indexes (spec/design/gist.md §5): the
// lowest-named GiST index whose range column at offset+ci has a `col && const` / `col @> const`
// conjunct. Factored out so the SELECT planner (detectScanBound) and the UPDATE/DELETE scan share
// the identical detection (the GIN precedent) — the mutations pass their indexes/columns at offset 0.
func detectGistBound(filter *rExpr, indexes []indexDef, columns []catColumn, offset int) *gistBoundPlan {
	for _, idx := range indexes {
		if idx.Kind != indexGist {
			continue
		}
		// The planner gather is single-operator: only a single-column GiST index accelerates a
		// `col && Q` / `col @> Q` / `col = Q` conjunct. A multi-column GiST index (an EXCLUDE backing
		// structure, gist.md §7) is probed only by the constraint, never the planner.
		if len(idx.Keys) != 1 {
			continue
		}
		ci := idx.firstColumn()
		colGlobal := offset + ci
		colTy := columns[ci].Type
		if colTy.IsRange() {
			// range_ops (GX1): a `col && Q` / `col @> Q` conjunct.
			if s, _, ok := gistMatch(filter, colGlobal); ok {
				return &gistBoundPlan{nameKey: strings.ToLower(idx.Name), strategy: s, colGlobal: colGlobal}
			}
		} else if isGistScalarType(colTy) {
			// scalar `=` opclass (GX2): a `col = Q` conjunct over a fixed-width keyable scalar.
			if _, _, ok := gistScalarMatch(filter, colGlobal); ok {
				return &gistBoundPlan{nameKey: strings.ToLower(idx.Name), strategy: gistEqual, colGlobal: colGlobal, scalarType: colTy.ScalarTy()}
			}
		}
	}
	return nil
}

// gistScalarMatch finds the first WHERE AND-chain conjunct a GiST scalar `=` opclass on colGlobal
// accelerates (spec/design/gist.md §6): `col = Q` where Q is a constant (re-evaluable per scan).
// Equality is commutative (the column may be either operand). <> and the inequalities are not
// accelerated (the `=` opclass has only the equal strategy). Returns the Equal strategy and the
// constant operand — used at plan time (strategy) and exec time (recover from plan.filter).
func gistScalarMatch(filter *rExpr, colGlobal int) (gistStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := gistScalarMatch(filter.lhs, colGlobal); ok {
			return s, q, true
		}
		return gistScalarMatch(filter.rhs, colGlobal)
	}
	if filter.kind == reCompare && filter.op == opEq {
		if isColumn(filter.lhs, colGlobal) && rexprIsConstant(filter.rhs) {
			return gistEqual, filter.rhs, true
		}
		if isColumn(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs) {
			return gistEqual, filter.lhs, true
		}
	}
	return 0, nil, false
}

// gistQueryOperand recovers a GiST bound's constant query operand from the live filter at exec time
// — gistMatch for range_ops (&&/@>), gistScalarMatch for the scalar `=` opclass. Centralizes the
// strategy dispatch so every scan site (SELECT / UPDATE / DELETE) recovers the operand uniformly.
func gistQueryOperand(filter *rExpr, gb *gistBoundPlan) (*rExpr, bool) {
	if gb.strategy == gistEqual {
		_, q, ok := gistScalarMatch(filter, gb.colGlobal)
		return q, ok
	}
	_, q, ok := gistMatch(filter, gb.colGlobal)
	return q, ok
}

// gistMatch finds the first WHERE AND-chain conjunct a GiST range_ops index on colGlobal accelerates
// (spec/design/gist.md §5): `col && Q` (overlap — symmetric) or `col @> Q` (contains — asymmetric,
// the column must be the LEFT operand; `Q @> col` is the non-accelerated <@). Q must be a constant.
// The other range operators stay full-scan this slice. Returns the strategy and the constant query
// operand — used at plan time (strategy) and exec time (recover from plan.filter).
func gistMatch(filter *rExpr, colGlobal int) (gistStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := gistMatch(filter.lhs, colGlobal); ok {
			return s, q, true
		}
		return gistMatch(filter.rhs, colGlobal)
	}
	if filter.kind == reRangeOp && len(filter.sargs) == 2 {
		a, b := filter.sargs[0], filter.sargs[1]
		switch filter.rop {
		case roOverlaps: // && — symmetric in its operands
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return gistOverlaps, b, true
			}
			if isColumn(b, colGlobal) && rexprIsConstant(a) {
				return gistOverlaps, a, true
			}
		case roContains: // @> — the indexed column must be the container (LEFT)
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return gistContains, b, true
			}
		}
	}
	return 0, nil, false
}

// isColumn reports whether e is a reference to the column at global scope index colGlobal.
func isColumn(e *rExpr, colGlobal int) bool {
	return e != nil && e.kind == reColumn && e.index == colGlobal
}

// rexprIsConstant reports whether e is evaluable without a current/outer row (so its value is the
// same for every scanned row — computable once). False for any column, correlated outer column, or
// subquery; true for literals, params, and pure operations over them. Used to admit a GIN query
// operand Q (spec/design/gin.md §6: a constant query only this slice). Mirrors the traversal of
// rexprReferencesOuter.
func rexprIsConstant(e *rExpr) bool {
	if e == nil {
		return true
	}
	switch e.kind {
	case reColumn, reOuterColumn, reSubquery:
		return false
	case reDateClock:
		// Row-independent but EXECUTION-scoped (the statement clock + session zone) —
		// conservatively not a "constant", so no plan-time consumer ever evaluates it without a
		// live statement environment (date.md §6).
		return false
	}
	if e.operand != nil && !rexprIsConstant(e.operand) {
		return false
	}
	if e.lhs != nil && !rexprIsConstant(e.lhs) {
		return false
	}
	if e.rhs != nil && !rexprIsConstant(e.rhs) {
		return false
	}
	for _, arm := range e.caseArms {
		if !rexprIsConstant(arm.cond) || !rexprIsConstant(arm.result) {
			return false
		}
	}
	if e.caseEls != nil && !rexprIsConstant(e.caseEls) {
		return false
	}
	for _, a := range e.sargs {
		if !rexprIsConstant(a) {
			return false
		}
	}
	for _, s := range e.subs {
		if !rexprIsConstant(s.index) || !rexprIsConstant(s.lower) || !rexprIsConstant(s.upper) {
			return false
		}
	}
	return true
}

// indexBoundRows executes an index access-predicate bound (cost.md §3 "index-bounded scan",
// indexes.md §5.1): build the concrete index-key range from the equality prefix + optional
// trailing range, then fetch the rows it admits, in index-entry order (= key order, then
// storage-key order), with the scan's up-front units (pages, slabs) — the index-tree nodes
// overlapping the range plus, per admitted entry, the table-tree nodes of that row's point
// lookup and its touched-column decompress slabs. The caller feeds the rows through the same
// scanSource as any bounded scan. A provably empty bound (a NULL / contradictory equality, a
// NULL / contradictory range, an out-of-range integer) returns nothing and charges nothing.
func (db *engine) indexBoundRows(tableName string, ib *indexBoundPlan, params []Value, outer []storedRow, mask []bool, left storedRow) (rows []storedRow, pages, slabs int, err error) {
	entries, pages, slabs, err := db.indexBoundEntries(tableName, ib, params, outer, mask, left)
	if err != nil {
		return nil, 0, 0, err
	}
	rows = make([]storedRow, len(entries))
	for i := range entries {
		rows[i] = entries[i].Row
	}
	return rows, pages, slabs, nil
}

// indexBoundEntries is the key-preserving form of indexBoundRows. Keeping the storage key beside
// each fetched row gives SELECT and mutation consumers one access-path result contract; SELECT's
// compatibility wrapper above discards the keys.
func (db *engine) indexBoundEntries(tableName string, ib *indexBoundPlan, params []Value, outer []storedRow, mask []bool, left storedRow) (entries []entry, pages, slabs int, err error) {
	b, prefixByteLen, empty := db.buildIndexBound(ib, params, outer, left)
	if empty {
		return nil, 0, 0, nil
	}
	return db.indexScanBoundEntries(tableName, ib.nameKey, ib.suffixTypes, b, prefixByteLen, mask)
}

// buildIndexBound turns an index access predicate into a concrete index-key range at exec time
// (indexes.md §5.1). It encodes the equality prefix into P (the concatenated present slots), then
// — if there is a range column — starts from [P, P‖0x01) (the upper endpoint stops before the
// range column's NULL slot, since a range is never true for NULL) and intersects each range term;
// otherwise the range is [P, byte-successor(P)) (every entry extending P). empty=true ⇒ the bound
// admits no key (a NULL / disagreeing prefix equality, a NULL range endpoint, or a contradictory
// range). prefixByteLen = len(P), the byte count the row-key suffix skip advances past the
// equality-prefix slots before width-skipping the remaining components.
func (db *engine) buildIndexBound(ib *indexBoundPlan, params []Value, outer []storedRow, left storedRow) (b keyBound, prefixByteLen int, empty bool) {
	var p []byte
	for _, ec := range ib.eqCols {
		// Every equality const-source on this column must encode to ONE agreed value: a NULL is
		// 3VL-never-true, a disagreement (`a = 1 AND a = 2`) is a contradiction, and an out-of-range
		// integer can equal no stored value — all provably empty.
		var agreed []byte
		for _, src := range ec.srcs {
			key, isNull, ok := encodeBoundKey(ec.colType, src, params, outer, ec.coll, left)
			if isNull || !ok {
				return keyBound{}, 0, true
			}
			if agreed == nil {
				agreed = key
			} else if !bytes.Equal(agreed, key) {
				return keyBound{}, 0, true
			}
		}
		p = append(p, 0x00)
		p = append(p, agreed...)
	}
	if ib.rangeTerms == nil {
		b = keyBound{lo: p, loInc: true, hi: prefixSuccessor(p), hiInc: false}
		return b, len(p), boundEmpty(b)
	}
	// Equality prefix P + a range on the next column. Base: [P, P‖0x01) — present values only
	// (the 0x01 NULL tag sorts after every 0x00 present slot at this position).
	b = keyBound{lo: append([]byte(nil), p...), loInc: true, hi: append(append([]byte(nil), p...), 0x01), hiInc: false}
	for _, t := range ib.rangeTerms {
		// The range column is fixed-width (indexes.md §5.1 eligibility), so it is never collated: the
		// probe encodes with a nil collation.
		key, isNull, ok := encodeBoundKey(ib.rangeType, t.src, params, outer, nil, left)
		if isNull {
			return keyBound{}, 0, true
		}
		if !ok {
			continue // out-of-range endpoint: drop this half-bound (a wider, still-sound scan)
		}
		ps := append(append([]byte(nil), p...), 0x00) // P ‖ 0x00 (present tag)
		ps = append(ps, key...)                       // P ‖ 0x00 ‖ encode(v)
		switch t.op {
		case opGe:
			b = intersectLo(b, ps, true)
		case opGt:
			b = intersectLo(b, prefixSuccessor(ps), true) // skip the whole c = v subtree
		case opLt:
			b = intersectHi(b, ps, false)
		case opLe:
			b = intersectHi(b, prefixSuccessor(ps), false)
		case opEq: // defensive — an equality never reaches rangeTerms, but treat it as [v, v]
			b = intersectLo(b, ps, true)
			b = intersectHi(b, prefixSuccessor(ps), false)
		}
	}
	return b, len(p), boundEmpty(b)
}

// indexScanBound range-scans the index B-tree over an already-built key bound and point-looks-up
// each admitted entry's row, returning them in index-entry order with the scan's up-front (pages,
// slabs) block — the index-tree nodes overlapping the range plus, per entry, the row's point
// lookup. prefixByteLen is the equality-prefix byte length skipped before the fixed-width
// suffix-skip that recovers each entry's row storage key (indexes.md §5.1). Shared by the
// access-predicate bound (indexBoundRows) and the OR / IN-list point-set (indexPointRows) so both
// drive the identical per-entry fetch — same cost by construction.
func (db *engine) indexScanBound(tableName, nameKey string, suffixTypes []scalarType, b keyBound, prefixByteLen int, mask []bool) (rows []storedRow, pages, slabs int, err error) {
	entries, pages, slabs, err := db.indexScanBoundEntries(tableName, nameKey, suffixTypes, b, prefixByteLen, mask)
	if err != nil {
		return nil, 0, 0, err
	}
	rows = make([]storedRow, len(entries))
	for i := range entries {
		rows[i] = entries[i].Row
	}
	return rows, pages, slabs, nil
}

// indexScanBoundEntries is the key-preserving core of the ordered-index gather. Candidate ordering
// and units are identical to indexScanBound; only the already-recovered storage key is retained.
func (db *engine) indexScanBoundEntries(tableName, nameKey string, suffixTypes []scalarType, b keyBound, prefixByteLen int, mask []bool) (out []entry, pages, slabs int, err error) {
	istore := db.lkpIndexStore(nameKey)
	// The index store has no payload columns, so its mask is empty and its fused scan contributes
	// only the index-tree page_read count (no spill/compress units).
	entries, pages, _, err := istore.RangeScanWithUnits(b, nil)
	if err != nil {
		return nil, 0, 0, err
	}
	store := db.lkpStore(tableName)
	for _, e := range entries {
		// Skip the equality prefix by its known byte length, then each remaining key component by
		// width (self-delimiting — a 0x01 NULL tag alone, or 0x00 + the fixed width, indexes.md §5.1);
		// the suffix after them is the row's storage key (indexes.md §3).
		at := prefixByteLen
		for _, ty := range suffixTypes {
			if at < len(e.Key) && e.Key[at] == 0x01 {
				at++
			} else {
				at += 1 + ty.WidthBytes()
			}
		}
		rowKey := e.Key[at:]
		row, ok, n, sl, err := store.GetWithUnits(rowKey, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		pages += n
		slabs += sl
		if !ok {
			panic("an index entry references a stored row")
		}
		out = append(out, entry{Key: append([]byte(nil), rowKey...), Row: row})
	}
	return out, pages, slabs, nil
}

// indexPointRows fetches the rows a SINGLE already-encoded leading-column index value admits — the
// equality prefix scan [0x00‖value, byte-successor) over the index B-tree plus, per admitted entry,
// the row's point lookup. Used by the OR / IN-list secondary-index point-set (indexKeySetRows),
// where each distinct list value is one such point probe. suffixTypes are the trailing key
// components (columns[1:], fixed-width), width-skipped past the single leading slot.
func (db *engine) indexPointRows(tableName, nameKey string, suffixTypes []scalarType, valueKey []byte, mask []bool) (rows []storedRow, pages, slabs int, err error) {
	entries, pages, slabs, err := db.indexPointEntries(tableName, nameKey, suffixTypes, valueKey, mask)
	if err != nil {
		return nil, 0, 0, err
	}
	rows = make([]storedRow, len(entries))
	for i := range entries {
		rows[i] = entries[i].Row
	}
	return rows, pages, slabs, nil
}

func (db *engine) indexPointEntries(tableName, nameKey string, suffixTypes []scalarType, valueKey []byte, mask []bool) (entries []entry, pages, slabs int, err error) {
	prefix := append([]byte{0x00}, valueKey...)
	b := keyBound{lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false}
	return db.indexScanBoundEntries(tableName, nameKey, suffixTypes, b, len(prefix), mask)
}

// encodeKeySet encodes an OR / IN-list's equality const-sources into the key space and returns
// a SORTED, DE-DUPLICATED set of encoded keys — the distinct point probes a merged point-set
// bound visits (cost.md §3 "OR / IN-list"). A src that is NULL (3VL-never-true) or does not
// encode into the key domain (an out-of-range integer — no stored key can equal it) contributes
// no point and is skipped (sound: the union stays a superset, and the residual filter re-checks
// each admitted row). Byte-dedup == value-dedup and byte-sort == value-sort under the
// order-preserving key encoding (encoding.md §2), so probing the sorted distinct keys yields
// rows in ascending key order with no row visited twice. Shared by the PK and secondary-index
// point-set executors.
func encodeKeySet(keyType scalarType, srcs []*rExpr, params []Value, outer []storedRow, coll *Collation, left storedRow) [][]byte {
	keys := make([][]byte, 0, len(srcs))
	for _, src := range srcs {
		key, isNull, ok := encodeBoundKey(keyType, src, params, outer, coll, left)
		if isNull || !ok {
			continue
		}
		keys = append(keys, key)
	}
	sort.Slice(keys, func(i, j int) bool { return bytes.Compare(keys[i], keys[j]) < 0 })
	out := keys[:0]
	for i, k := range keys {
		if i == 0 || !bytes.Equal(k, keys[i-1]) {
			out = append(out, k)
		}
	}
	return out
}

// pkKeySetRows executes a primary-key OR / IN-list point-set bound (cost.md §3 "OR / IN-list"):
// each distinct encoded key is a point probe [k, k] over the row's own B-tree, and the admitted
// rows are concatenated in ascending key order (the same order a full scan would yield). The
// per-probe (pages, slabs) blocks sum, so the metered cost is the sum of the individual point
// lookups — a core that full-scans instead computes a different cost (the cross-core contract,
// §8). Returns the (key, row) entries so the mutation paths can Remove/Replace by key; SELECT
// discards the keys. Reconstruction is uniformly lazy since B4 (bplus-reshape.md) — the old
// masked/unmasked reconstruction variants are collapsed, and the (pages, slabs) cost stays driven
// by the shared mask (the cost touched set).
func (db *engine) pkKeySetRows(store *tableStore, ks *pkKeySetPlan, params []Value, outer []storedRow, mask []bool, left storedRow, masked bool) (entries []entry, pages, slabs int, err error) {
	for _, k := range encodeKeySet(ks.pkType, ks.srcs, params, outer, ks.coll, left) {
		b := keyBound{lo: k, loInc: true, hi: k, hiInc: true}
		es, p, sl, err := store.RangeScanWithUnits(b, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		entries = append(entries, es...)
		pages += p
		slabs += sl
	}
	return entries, pages, slabs, nil
}

// indexKeySetRows executes a secondary-index OR / IN-list point-set bound (cost.md §3 "OR /
// IN-list"): each distinct encoded value is an index point probe (indexPointRows), and the rows
// are gathered in ascending value order. Because a row has exactly one value for the indexed
// column, distinct values probe disjoint row sets — no row is fetched twice. The per-probe
// (pages, slabs) blocks sum, matching the PK point-set cost contract.
func (db *engine) indexKeySetRows(tableName string, ks *indexKeySetPlan, params []Value, outer []storedRow, mask []bool, left storedRow) (rows []storedRow, pages, slabs int, err error) {
	entries, pages, slabs, err := db.indexKeySetEntries(tableName, ks, params, outer, mask, left)
	if err != nil {
		return nil, 0, 0, err
	}
	rows = make([]storedRow, len(entries))
	for i := range entries {
		rows[i] = entries[i].Row
	}
	return rows, pages, slabs, nil
}

func (db *engine) indexKeySetEntries(tableName string, ks *indexKeySetPlan, params []Value, outer []storedRow, mask []bool, left storedRow) (entries []entry, pages, slabs int, err error) {
	for _, k := range encodeKeySet(ks.colType, ks.srcs, params, outer, ks.coll, left) {
		r, p, sl, err := db.indexPointEntries(tableName, ks.nameKey, ks.tailTypes, k, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		entries = append(entries, r...)
		pages += p
		slabs += sl
	}
	return entries, pages, slabs, nil
}

// executeMutationScan executes a planned UPDATE/DELETE access path into the normalized keyed-row
// batch. It owns the access-method switch that used to be duplicated inline in both DML executors;
// per-row guards, storage_row_read, residual evaluation, and the phase-2 writes stay with the caller.
func (db *engine) executeMutationScan(plan mutationScanPlan, tableName string, params []Value, env *evalEnv, meter *costMeter, mask []bool) (mutationScanBatch, error) {
	store := db.lkpStoreScoped(plan.scope, tableName)
	b := plan.bound
	if b == nil {
		entries, pages, slabs, err := store.ScanWithUnits(mask)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.pk != nil {
		kb, empty := db.buildKeyBound(b.pk, params, nil, nil)
		if empty {
			return mutationScanBatch{empty: true}, nil
		}
		entries, pages, slabs, err := store.RangeScanWithUnits(kb, mask)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.index != nil {
		entries, pages, slabs, err := db.indexBoundEntries(tableName, b.index, params, nil, mask, nil)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.gin != nil {
		var query *rExpr
		if _, q, ok := ginMatch(plan.filter, b.gin.colGlobal); ok {
			query = q
		}
		entries, pages, slabs, err := db.ginBoundRows(tableName, b.gin, query, env, meter, mask)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.gist != nil {
		var query *rExpr
		if q, ok := gistQueryOperand(plan.filter, b.gist); ok {
			query = q
		}
		entries, pages, slabs, err := db.gistBoundRows(tableName, b.gist, query, env, meter, mask)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.pkSet != nil {
		entries, pages, slabs, err := db.pkKeySetRows(store, b.pkSet, params, nil, mask, nil, false)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.indexSet != nil {
		entries, pages, slabs, err := db.indexKeySetEntries(tableName, b.indexSet, params, nil, mask, nil)
		if err != nil {
			return mutationScanBatch{}, err
		}
		// Retain first-probe order while guaranteeing that phase 2 can never receive the same row
		// twice if a future index-key generalization makes point-probe result sets overlap.
		seen := make(map[string]struct{}, len(entries))
		out := entries[:0]
		for _, e := range entries {
			key := string(e.Key)
			if _, ok := seen[key]; ok {
				continue
			}
			seen[key] = struct{}{}
			out = append(out, e)
		}
		return mutationScanBatch{entries: out, pages: pages, slabs: slabs}, nil
	}
	entries, pages, slabs, err := store.ScanWithUnits(mask)
	return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
}

// ginBoundRows executes a GIN-bounded scan (spec/design/gin.md §6, cost.md §3). Evaluates the
// constant query operand, extracts its terms + mode via the array_ops opclass (an array for @>/&&/=;
// a single scalar term for = ANY — ginMember; the array's distinct non-NULL terms for = — ginEqual),
// gathers each term's posting list (a prefix range scan of the GIN entry tree), combines them by mode
// (@>, = ANY, and = → intersection, && → union) into the candidate storage-key set, and
// point-looks-up each candidate in storage-key order. The original predicate stays the residual WHERE
// filter (re-applied downstream), so the result is always correct. Returns the candidate rows + the
// scan's up-front (pages, slabs); gin_entry (per posting entry visited) is charged on meter directly.
// Degenerate constant queries (gin.md §6): a NULL Q, an @> whose Q holds a NULL element, an && with
// no non-NULL term, and a NULL = ANY scalar are provably empty; @> '{}' and array = with no non-NULL
// term fall back to the full scan.
// ginBoundRows gathers a GIN-bounded scan's candidate rows as (storage key, row) Entry pairs
// (the candidate set IS the storage keys), with the up-front (page_read nodes, value_decompress
// slabs) block. SELECT drops the keys; UPDATE/DELETE keep them to rewrite/remove the rows
// (gin.md §6). GinEntry is charged inside (during the gather); the caller charges the block.
func (db *engine) ginBoundRows(tableName string, gb *ginBoundPlan, query *rExpr, env *evalEnv, meter *costMeter, mask []bool) (out []entry, pages, slabs int, err error) {
	store := db.lkpStore(tableName)
	if query == nil {
		return nil, 0, 0, nil
	}
	// Extract the query's terms (extract_query_terms) — a pure planning step, NOT metered (cost.md
	// §3): evaluate Q on a scratch meter. Q is a constant, so the nil row suffices.
	qv, err := query.eval(nil, env, &costMeter{})
	if err != nil {
		return nil, 0, 0, err
	}
	// Each term is the element's order-preserving key encoding (gin.md §4) — the SAME bytes the
	// entries carry, so a term doubles as its posting-list prefix below. Encoding now lets us dedup
	// distinct terms by bytes (a bijection: byte-dedup == value-dedup, byte-sort == value-sort)
	// generically over every admitted element type.
	var terms [][]byte
	hasNull := false
	isEmpty := false
	if gb.strategy == ginMember {
		// `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL c can equal no element,
		// so the bound is provably empty (gin.md §6). c is in the element type's domain by resolution
		// (jed coerces c to the element type, rejecting an out-of-range integer constant 22003 before
		// exec); InRange is a defensive guard against silently truncating an out-of-range integer into
		// a wrong term.
		if qv.Kind == ValNull {
			return nil, 0, 0, nil
		}
		if qv.Kind == ValInt && !gb.elemType.InRange(qv.Int) {
			return nil, 0, 0, nil // out-of-range guard
		}
		// a GIN element is fixed-width (isGinElementType excludes text), so the term never collates / fails
		t, err := encodeKeyValue(gb.elemType, qv, nil)
		if err != nil {
			panic("a GIN element key is infallible (fixed-width, no collation)")
		}
		terms = append(terms, t)
	} else {
		if qv.Kind != ValArray {
			return nil, 0, 0, nil // a NULL whole-array (or non-array) query → provably empty
		}
		seen := make(map[string]bool)
		for _, el := range qv.arrayVal().Elements {
			if el.Kind == ValNull {
				hasNull = true // a NULL element carries no term
				continue
			}
			t, err := encodeKeyValue(gb.elemType, el, nil)
			if err != nil {
				panic("a GIN element key is infallible (fixed-width, no collation)")
			}
			if !seen[string(t)] {
				seen[string(t)] = true
				terms = append(terms, t)
			}
		}
		isEmpty = len(qv.arrayVal().Elements) == 0
		slices.SortFunc(terms, bytes.Compare)
	}

	switch gb.strategy {
	case ginContains:
		if isEmpty {
			// @> '{}': every non-NULL array contains the empty array — not derivable from the index;
			// fall back to the full scan (the residual filter keeps the right rows — gin.md §6).
			entries, p, sl, e := store.ScanWithUnits(mask)
			if e != nil {
				return nil, 0, 0, e
			}
			return entries, p, sl, nil
		}
		if hasNull {
			return nil, 0, 0, nil // @> a query with a NULL element is never TRUE
		}
	case ginEqual:
		if len(terms) == 0 {
			// col = Q with NO non-NULL term — '{}' (isEmpty) or an all-NULL Q (hasNull, no non-NULL
			// element). The rows it matches ({}, {NULL}, …) carry NO index terms, so the index cannot
			// enumerate them: fall back to the full scan and let the residual = keep them (gin.md §6).
			// NOT a provably-empty bound — and a Q with ≥1 non-NULL element is NOT caught here (it
			// gathers, even when it also has a NULL element).
			entries, p, sl, e := store.ScanWithUnits(mask)
			if e != nil {
				return nil, 0, 0, e
			}
			return entries, p, sl, nil
		}
	case ginOverlaps:
		if len(terms) == 0 {
			return nil, 0, 0, nil // && with no non-NULL term overlaps nothing
		}
	}

	// Gather each term's posting list: the entry range [encode(term), successor) of the GIN tree
	// (gin.md §4). The entry is encode(term) ‖ storage_key; the fixed-width term self-delimits, so
	// the storage key is the suffix after termWidth bytes.
	istore := db.lkpIndexStore(gb.nameKey)
	termWidth := gb.elemType.WidthBytes()
	entriesVisited := 0
	postings := make([][][]byte, 0, len(terms))
	for _, prefix := range terms {
		b := keyBound{lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false}
		es, p, _, e := istore.RangeScanWithUnits(b, nil)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += p
		entriesVisited += len(es)
		keys := make([][]byte, len(es))
		for i := range es {
			keys[i] = es[i].Key[termWidth:]
		}
		postings = append(postings, keys)
	}
	meter.Charge(costs.GinEntry * int64(entriesVisited))

	// Combine into the candidate storage keys, ascending byte (= storage-key) order, so the point
	// lookups and emitted rows follow storage order exactly as a full scan (gin.md §6/§8).
	// @> ALL → intersection; = ANY (ginMember) is a single term, so its intersection is that lone
	// posting list; array = (ginEqual) gathers the same superset as @> over Q's distinct non-NULL
	// terms (the residual = makes it exact downstream). && ANY → union.
	var cand [][]byte
	if gb.strategy == ginOverlaps {
		cand = unionPostings(postings)
	} else {
		cand = intersectPostings(postings)
	}

	for _, key := range cand {
		row, ok, n, sl, e := store.GetWithUnits(key, mask)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += n
		slabs += sl
		if !ok {
			panic("a GIN entry references a stored row")
		}
		out = append(out, entry{Key: key, Row: row})
	}
	return out, pages, slabs, nil
}

// gistBoundRows gathers a GiST-bounded scan's candidate rows (spec/design/gist.md §5). Evaluates the
// constant query operand, then DESCENDS the index's resident R-tree visiting only children
// consistent with the query, collecting candidate storage keys at the leaves; each candidate row is
// point-looked-up in storage-key order. The original &&/@> predicate stays the residual WHERE filter
// (always-recheck), so the result is exactly the full-scan result — the bound only narrows which rows
// are fetched. Returns the candidate (key, row) Entry pairs + the up-front (page_read, value_decompress)
// block (tree nodes visited + each candidate's lookup); gist_descent (per interior) is charged on meter
// directly. Degenerate constant queries (gist.md §5): a NULL Q and an empty && query match nothing; an
// empty @> query (col @> 'empty') matches every row → full-scan fallback (the empty bound is invisible
// to the overlap-descend).
func (db *engine) gistBoundRows(tableName string, gb *gistBoundPlan, query *rExpr, env *evalEnv, meter *costMeter, mask []bool) (out []entry, pages, slabs int, err error) {
	store := db.lkpStore(tableName)
	if query == nil {
		return nil, 0, 0, nil
	}
	// The query operand is a constant; evaluating it (extract query) is a planning step, NOT metered.
	qv, err := query.eval(nil, env, &costMeter{})
	if err != nil {
		return nil, 0, 0, err
	}
	// Form the resident-tree search query from the constant, handling strategy-specific degenerate
	// cases. A NULL query is never TRUE for any row (all strategies).
	var gq gistQuery
	if gb.strategy == gistEqual {
		// scalar `=` (gist.md §6): encode the constant to its order-preserving key bytes.
		if qv.Kind == ValNull {
			return nil, 0, 0, nil
		}
		k, e := encodeKeyValue(gb.scalarType, qv, nil)
		if e != nil {
			panic("a fixed-width GiST scalar key is infallible (no collation)")
		}
		gq = gistQuery{skey: k}
	} else {
		if qv.Kind != ValRange {
			return nil, 0, 0, nil // a NULL (or non-range) query is never TRUE (both && and @>)
		}
		qr := qv.rangeVal()
		if qr.Empty {
			switch gb.strategy {
			case gistContains:
				// col @> 'empty' is TRUE for every row, but an empty bound is absorbed by the union, so
				// it is invisible to the overlap-descend (a false-negative trap, gist.md §5). Fall back
				// to the full scan; the residual @> keeps every row.
				entries, p, sl, e := store.ScanWithUnits(mask)
				return entries, p, sl, e
			default:
				return nil, 0, 0, nil // col && 'empty' overlaps nothing
			}
		}
		gq = gistQuery{rng: qr}
	}
	// Descend the resident R-tree (rebuilt at each mutating statement, gist.md §3/§4.1) — no per-query
	// build. page_read per node touched + gist_descent per interior node.
	var skeys [][]byte
	if tree := db.readSnap().gistTreeFor(gb.nameKey); tree != nil {
		nodes, interior := 0, 0
		skeys, nodes, interior = tree.search([]gistQuery{gq}, []gistStrategy{gb.strategy})
		pages += nodes
		meter.Charge(costs.GistDescent * int64(interior))
	}
	// Point-look-up each candidate in storage-key order (the candidates ARE storage keys).
	slices.SortFunc(skeys, bytes.Compare)
	skeys = slices.CompactFunc(skeys, bytes.Equal)
	for _, key := range skeys {
		row, ok, n, sl, e := store.GetWithUnits(key, mask)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += n
		slabs += sl
		if !ok {
			panic("a GiST entry references a stored row")
		}
		out = append(out, entry{Key: key, Row: row})
	}
	return out, pages, slabs, nil
}

// intersectPostings returns the storage keys present in EVERY posting list (the @> mode-ALL
// combine), sorted ascending. Each posting list holds distinct keys (one (term,row) entry per
// row), so a per-list count == the number of lists means the key is in all of them.
func intersectPostings(postings [][][]byte) [][]byte {
	if len(postings) == 0 {
		return nil
	}
	count := make(map[string]int)
	for _, list := range postings {
		for _, k := range list {
			count[string(k)]++
		}
	}
	need := len(postings)
	var out [][]byte
	for _, k := range postings[0] {
		if count[string(k)] == need {
			out = append(out, k)
		}
	}
	slices.SortFunc(out, bytes.Compare)
	return out
}

// unionPostings returns the storage keys present in ANY posting list (the && mode-ANY combine),
// deduplicated and sorted ascending.
func unionPostings(postings [][][]byte) [][]byte {
	seen := make(map[string]bool)
	var out [][]byte
	for _, list := range postings {
		for _, k := range list {
			if !seen[string(k)] {
				seen[string(k)] = true
				out = append(out, k)
			}
		}
	}
	slices.SortFunc(out, bytes.Compare)
	return out
}

// prefixSuccessor is the byte-successor of a prefix: the smallest byte string greater
// than every string that extends p. Increment the last non-0xFF byte and truncate after
// it; an all-0xFF prefix has no successor (nil ⇒ unbounded high end).
func prefixSuccessor(p []byte) []byte {
	s := append([]byte(nil), p...)
	for len(s) > 0 {
		if s[len(s)-1] == 0xFF {
			s = s[:len(s)-1]
		} else {
			s[len(s)-1]++
			return s
		}
	}
	return nil
}

// detectPKBound flattens the WHERE's top-level AND-chain (an OR is never descended — a disjunction
// is not a contiguous range) and collects every `pk <cmp> const-source` conjunct. Returns nil when
// none exist (⇒ full scan). Conservative + sound: an unrecognized conjunct contributes no bound and
// stays in the residual filter. coll is the key column's collation when collated AND Full (nil for a
// C key); a comparison's own collation must match it for the comparison to seed a bound.
func detectPKBound(filter *rExpr, pkIdx int, pkType scalarType, coll *Collation) *pkBoundPlan {
	colColl := ""
	if coll != nil {
		colColl = coll.Name
	}
	var terms []boundTerm
	var walk func(e *rExpr)
	walk = func(e *rExpr) {
		if e.kind == reAnd {
			walk(e.lhs)
			walk(e.rhs)
			return
		}
		if t, ok := asBoundTerm(e, columnMatch(pkIdx), pkType, colColl, -1); ok {
			terms = append(terms, t)
		}
	}
	walk(filter)
	if len(terms) == 0 {
		return nil
	}
	return &pkBoundPlan{pkType: pkType, terms: terms, coll: coll}
}

// keyMatch is what a bound's key operand is (spec/design/indexes.md §5): a plain column at a
// global ordinal (the PK bound and a column index key), or a resolved index EXPRESSION matched
// structurally against a WHERE conjunct operand (an expression index key). For the expression
// form, the key's Column(i) is table-local and matches a WHERE Column(i + offset). Go has no sum
// types: expr == nil discriminates the column form (read col) from the expression form.
type keyMatch struct {
	col    int
	expr   *rExpr
	offset int
}

func columnMatch(globalOrdinal int) keyMatch  { return keyMatch{col: globalOrdinal} }
func exprMatch(e *rExpr, offset int) keyMatch { return keyMatch{expr: e, offset: offset} }

// matches reports whether a WHERE conjunct operand x is this key's operand.
func (k keyMatch) matches(x *rExpr) bool {
	if k.expr == nil {
		return x.kind == reColumn && x.index == k.col
	}
	return rexprEqShifted(x, k.expr, k.offset)
}

// rexprEqShifted is a SOUND-if-incomplete structural equality for index-expression matching
// (spec/design/indexes.md §5): does the WHERE conjunct operand a (GLOBAL column indices) equal the
// resolved index key expression b (table-local Column(i), matched as Column(i + offset))? Covers
// the common index-expression shapes; any unrecognized / typmod-bearing shape returns false — a
// missed bound is always sound (a full scan + residual filter), matching PostgreSQL's syntactic
// (not semantic) index-expression matching.
func rexprEqShifted(a, b *rExpr, offset int) bool {
	if a == nil || b == nil {
		return a == b
	}
	if a.kind != b.kind {
		return false
	}
	switch a.kind {
	case reColumn:
		return a.index == b.index+offset
	case reConstInt:
		return a.cInt == b.cInt
	case reConstBool:
		return a.cBool == b.cBool
	case reConstText:
		return a.cText == b.cText
	case reConstNull:
		return true
	case reScalarFunc:
		if a.sfunc != b.sfunc || len(a.sargs) != len(b.sargs) {
			return false
		}
		for i := range a.sargs {
			if !rexprEqShifted(a.sargs[i], b.sargs[i], offset) {
				return false
			}
		}
		return true
	case reCoalesce:
		// COALESCE(a, b, …) is a legal (immutable-iff-args-are) index expression (grammar.md
		// §51), so an index on COALESCE(x, 0) must match the same spelling in a query.
		if a.caseDecimal != b.caseDecimal || len(a.sargs) != len(b.sargs) {
			return false
		}
		for i := range a.sargs {
			if !rexprEqShifted(a.sargs[i], b.sargs[i], offset) {
				return false
			}
		}
		return true
	case reGreatestLeast:
		// GREATEST/LEAST(a, b, …) is likewise a legal index expression (grammar.md §52); a
		// GREATEST index must not match a LEAST query (the `greatest` discriminant is compared),
		// nor an index built under a different text collation (collationsEqual — a collation-X
		// index must not answer a collation-Y query).
		if a.greatest != b.greatest || a.caseDecimal != b.caseDecimal ||
			!collationsEqual(a.collation, b.collation) || len(a.sargs) != len(b.sargs) {
			return false
		}
		for i := range a.sargs {
			if !rexprEqShifted(a.sargs[i], b.sargs[i], offset) {
				return false
			}
		}
		return true
	case reArith:
		return a.op == b.op &&
			rexprEqShifted(a.lhs, b.lhs, offset) &&
			rexprEqShifted(a.rhs, b.rhs, offset)
	case reCast:
		// A scalar cast, no typmod / varchar(n) (those would change the value's byte form).
		return a.typmod == nil && b.typmod == nil && a.varchar == nil && b.varchar == nil &&
			a.result == b.result && rexprEqShifted(a.operand, b.operand, offset)
	case reNeg:
		return rexprEqShifted(a.operand, b.operand, offset)
	case reNot:
		return rexprEqShifted(a.operand, b.operand, offset)
	case reCasing:
		// lower(x)/upper(x) (spec/design/collation.md §16) resolve to a dedicated reCasing node —
		// NOT reScalarFunc — so an index on lower(email) (the headline expression-index shape)
		// matches ONLY if this arm is present. The fold is deterministic (engine-global casing
		// regime, identical at index-build and query-eval), so the match is sound: same direction
		// + a matching argument.
		return a.casingUpper == b.casingUpper && rexprEqShifted(a.operand, b.operand, offset)
	case reCompare:
		// A comparison (status = 'active', amt > 0) is the canonical partial-index predicate shape
		// (indexes.md §9): same operator + same derived collation + structurally-equal operands.
		return a.op == b.op && collationsEqual(a.collation, b.collation) &&
			rexprEqShifted(a.lhs, b.lhs, offset) && rexprEqShifted(a.rhs, b.rhs, offset)
	case reAnd, reOr:
		return rexprEqShifted(a.lhs, b.lhs, offset) && rexprEqShifted(a.rhs, b.rhs, offset)
	case reIsNull:
		return a.negated == b.negated && rexprEqShifted(a.operand, b.operand, offset)
	default:
		return false
	}
}

// collationsEqual reports whether two derived collations are the same (both nil / C, or both a
// loaded collation of the same name) — used to compare comparison nodes in rexprEqShifted.
func collationsEqual(a, b *Collation) bool {
	if a == nil || b == nil {
		return a == b
	}
	return a.Name == b.Name
}

// filterImpliesPredicate reports whether the WHERE filter implies a PARTIAL index's predicate
// (spec/design/indexes.md §9). jed's syntactic test (PG's, not a prover): every top-level conjunct
// of pred must be present as a top-level conjunct of filter (so a conjunctive predicate a AND b is
// implied by a WHERE that lists both a and b). pred is in table-local column coords; a filter
// conjunct is global, so it is matched shifted by offset. Sound-if-conservative: a miss means the
// index is not used (a correct full scan + residual filter).
func filterImpliesPredicate(filter, pred *rExpr, offset int) bool {
	if pred.kind == reAnd {
		return filterImpliesPredicate(filter, pred.lhs, offset) &&
			filterImpliesPredicate(filter, pred.rhs, offset)
	}
	// filter contains a top-level conjunct structurally equal to pred.
	var contains func(f *rExpr) bool
	contains = func(f *rExpr) bool {
		if f.kind == reAnd {
			return contains(f.lhs) || contains(f.rhs)
		}
		return rexprEqShifted(f, pred, offset)
	}
	return contains(filter)
}

// asBoundTerm recognizes a single comparison conjunct: a comparison (=,<,<=,>,>=) whose one side
// matches the bound's key operand (a bare LOCAL column at key.col, or the resolved index
// expression — a correlated reOuterColumn is a different kind, so it never matches) and whose
// other side is a const-source of the key's own type (a promoted comparison — e.g. intpk = 2.5 → a
// reConstDecimal — does not match, so it stays residual). The op is flipped when the key is on the
// right.
func asBoundTerm(e *rExpr, key keyMatch, pkType scalarType, colColl string, siblingCutoff int) (boundTerm, bool) {
	if e.kind != reCompare {
		return boundTerm{}, false
	}
	// A comparison bounds the key only when ITS resolved collation matches the key column's frozen
	// collation (colColl) — so the comparison orders text the SAME way the B-tree is keyed
	// (spec/design/collation.md §8). C key ⇔ a C/byte comparison (both empty); a collated key ⇔ a
	// comparison under the SAME collation (the column's implicit collation, or an explicit
	// COLLATE "<that name>"). A comparison under a DIFFERENT collation — name COLLATE "C" over a
	// unicode column, COLLATE "de" over unicode — does NOT match: its order disagrees with the
	// stored keys, so it stays a full scan + residual filter. (A *skewed* collated key never reaches
	// here — keyCollationCtx refuses the whole bound, §12.) The probe is then encoded in the key
	// column's form (sort key for a Full-collated column — buildKeyBound/indexBoundRows).
	cmpColl := ""
	if e.collation != nil {
		cmpColl = e.collation.Name
	}
	if cmpColl != colColl {
		return boundTerm{}, false
	}
	switch e.op {
	case opEq, opLt, opLe, opGt, opGe:
	default:
		return boundTerm{}, false
	}
	isKey := func(x *rExpr) bool { return key.matches(x) }
	switch {
	case isKey(e.lhs) && isConstSource(e.rhs, pkType, siblingCutoff):
		return boundTerm{op: e.op, src: e.rhs}, true
	case isKey(e.rhs) && isConstSource(e.lhs, pkType, siblingCutoff):
		return boundTerm{op: flipCompare(e.op), src: e.lhs}, true
	}
	return boundTerm{}, false
}

// isConstSource reports whether e is constant for the whole scan (no per-row input) AND of a type
// that encodes into the PK key space: a same-family const literal, a NULL literal (⇒ a provably
// empty range), a bind parameter $N (its inferred type matched the PK via the comparison; a value
// that does not fit is caught at buildKeyBound), or a bare correlated reOuterColumn — its value is a
// runtime constant for a given outer row, so the inner subquery's PK is bounded by the current outer
// row's column and seeks instead of re-scanning the whole inner table per outer row (cost.md §3
// "bounded scan", grammar.md §26). A type-mismatched outer reference is wrapped in a cast by the
// resolver (as for a const literal), so it never arrives here bare — the type check stays implicit.
//
// siblingCutoff opens the index-nested-loop door (cost.md §3 "JOIN"): when >= 0, a bare reColumn
// whose GLOBAL index is < siblingCutoff — a column of an EARLIER join relation — is a valid bound
// source, resolved per outer row from the combined left-hand row (like reOuterColumn, a bare sibling
// column implies a type match — a mismatch is a cast, never bare). -1 (the ordinary once-materialized
// bound) accepts only literals/params/outer references.
func isConstSource(e *rExpr, pkType scalarType, siblingCutoff int) bool {
	switch e.kind {
	case reParam, reConstNull, reOuterColumn:
		return true
	case reColumn:
		return siblingCutoff >= 0 && e.index < siblingCutoff
	case reConstInt:
		return pkType.IsInteger()
	case reConstBool:
		return pkType.IsBool()
	case reConstUuid:
		return pkType.IsUuid()
	case reConstTimestamp:
		return pkType.IsTimestamp()
	case reConstTimestamptz:
		return pkType.IsTimestamptz()
	case reConstDate:
		return pkType.IsDate()
	case reConstText:
		return pkType.IsText()
	case reConstBytea:
		return pkType.IsBytea()
	case reConstDecimal:
		return pkType.IsDecimal()
	case reConstInterval:
		return pkType.IsInterval()
	}
	return false
}

// flipCompare swaps a comparison's sense (for `const <op> pk` ⇒ `pk <flipped> const`). Eq is
// symmetric.
func flipCompare(op binaryOp) binaryOp {
	switch op {
	case opLt:
		return opGt
	case opLe:
		return opGe
	case opGt:
		return opLt
	case opGe:
		return opLe
	default:
		return op
	}
}

// buildKeyBound turns the plan-time terms into a concrete key range at exec time: encode each
// const-source in the PK key space and intersect the half-bounds. empty=true ⇒ the range admits no
// key (a NULL const — 3VL — or contradictory bounds like pk>5 AND pk<5), so the scan reads nothing
// and charges nothing. An out-of-range integer const drops only its own half-bound (a wider, still
// sound, scan), never a wrong key.
// outer carries the enclosing rows (innermost last) so a correlated reOuterColumn source resolves to
// the current outer row's value; it is nil for a top-level statement.
func (db *engine) buildKeyBound(bp *pkBoundPlan, params []Value, outer []storedRow, left storedRow) (keyBound, bool) {
	b := unboundedBound()
	for _, t := range bp.terms {
		key, isNull, ok := encodeBoundKey(bp.pkType, t.src, params, outer, bp.coll, left)
		if isNull {
			return keyBound{}, true
		}
		if !ok {
			continue
		}
		switch t.op {
		case opEq:
			b = intersectLo(b, key, true)
			b = intersectHi(b, key, true)
		case opGt:
			b = intersectLo(b, key, false)
		case opGe:
			b = intersectLo(b, key, true)
		case opLt:
			b = intersectHi(b, key, false)
		case opLe:
			b = intersectHi(b, key, true)
		}
	}
	if boundEmpty(b) {
		return keyBound{}, true
	}
	return b, false
}

// encodeBoundKey encodes a const-source's value into the PK's storage key (the same codec INSERT
// uses — EncodeInt for integer/timestamp widths, the raw 16 bytes for uuid, the 1-byte bool-byte
// for boolean). isNull ⇒ the value is NULL; ok=false (not null) ⇒ an integer value outside the PK
// type's range (no key can equal it), so the caller drops this bound. reParam/reOuterColumn resolve
// to a runtime Value first (the param table / the enclosing outer row) and then encode through the
// shared path.
func encodeBoundKey(pkType scalarType, src *rExpr, params []Value, outer []storedRow, coll *Collation, left storedRow) (key []byte, isNull bool, ok bool) {
	switch src.kind {
	case reConstNull:
		return nil, true, false
	case reConstInt:
		if !pkType.InRange(src.cInt) {
			return nil, false, false
		}
		return encodeInt(pkType, src.cInt), false, true
	case reConstBool:
		return encodeBool(src.cBool), false, true
	case reConstUuid:
		return src.cBytea, false, true
	case reConstTimestamp, reConstTimestamptz:
		return encodeInt(pkType, src.cInt), false, true
	case reConstText:
		return encodeTextBound(src.cText, coll)
	case reConstBytea:
		return encodeTerminated(src.cBytea), false, true
	case reConstDecimal:
		return src.cDec.EncodeKey(), false, true
	case reConstInterval:
		return src.cIv.EncodeKey(), false, true
	case reParam:
		return encodeValueKey(pkType, params[src.index], coll)
	case reOuterColumn:
		// A correlated reference: column index of the enclosing row level hops out — the same
		// indexing the evaluator uses for reOuterColumn (innermost outer row is last).
		return encodeValueKey(pkType, outer[len(outer)-src.level][src.index], coll)
	case reColumn:
		// Index-nested-loop: the GLOBAL column index of an earlier join relation, read from the
		// current combined left-hand row (cost.md §3 "JOIN"). The join loop always passes a `left`
		// wide enough (the running row spans columns [0, rel.offset), and a sibling index is <
		// rel.offset); a stray out-of-range index widens to a full scan rather than panic.
		if src.index >= len(left) {
			return nil, false, false
		}
		return encodeValueKey(pkType, left[src.index], coll)
	}
	return nil, false, false
}

// encodeTextBound encodes a text probe into a key bound: the raw text-terminated-escape bytes for a
// C key (coll == nil, the fast path, encoding.md §2.4), or the collation's UCA sort key
// (text-collated-sortkey, §2.12) for a Full-collated key. A sort-key build that fails on an unmapped
// code point (the 0A000 the write/compare path raises, collation.md §6) yields ok=false here: the
// probe matches no stored (always-mapped) key, so the term contributes no bound and the scan widens
// to a full scan + residual filter — which reproduces the exact non-pushdown answer (empty for =,
// since equality is byte-identity §7; the 0A000 for an ordering compare iff any row is scanned).
// Identical across cores (mirrors Rust encode_text_bound / TS encodeTextBound).
func encodeTextBound(s string, coll *Collation) (key []byte, isNull bool, ok bool) {
	if coll == nil {
		return encodeTerminated([]byte(s)), false, true
	}
	k, err := sortKey(coll, s)
	if err != nil {
		return nil, false, false
	}
	return k, false, true
}

// encodeValueKey encodes a runtime Value (a bound param or a resolved outer column) into the PK's
// storage key. isNull ⇒ the value is NULL (a 3VL-empty range); ok=false (not null) ⇒ an integer
// outside the PK width, so the caller drops this half-bound (a wider, still sound, scan). coll
// selects a text value's key form (collated sort key vs raw bytes — encodeTextBound).
func encodeValueKey(pkType scalarType, v Value, coll *Collation) (key []byte, isNull bool, ok bool) {
	if v.IsNull() {
		return nil, true, false
	}
	switch {
	case pkType.IsBool():
		return encodeBool(v.boolVal()), false, true
	case pkType.IsUuid():
		return []byte(v.str()), false, true
	case pkType.IsText():
		return encodeTextBound(v.str(), coll)
	case pkType.IsBytea():
		return encodeTerminated([]byte(v.str())), false, true
	case pkType.IsDecimal():
		if v.Kind != ValDecimal {
			return nil, false, false // mismatched param kind: drop this half-bound (sound widening)
		}
		return v.decimal().EncodeKey(), false, true
	case pkType.IsInterval():
		if v.Kind != ValInterval {
			return nil, false, false // mismatched param kind: drop this half-bound (sound widening)
		}
		return v.interval().EncodeKey(), false, true
	case pkType.IsFloat():
		// A float PK does NOT push down this slice (full-scan + residual filter, like the container
		// keys) — drop the half-bound (sound widening), matching Rust encode_value_key's OutOfRange.
		return nil, false, false
	case pkType.IsInteger():
		if !pkType.InRange(v.Int) {
			return nil, false, false
		}
		return encodeInt(pkType, v.Int), false, true
	default: // timestamp / timestamptz / date
		return encodeInt(pkType, v.Int), false, true
	}
}

// intersectLo tightens b's lower bound to the more restrictive of (current, key); at an equal key an
// exclusive bound (inc=false) wins.
func intersectLo(b keyBound, key []byte, inc bool) keyBound {
	if b.lo == nil {
		b.lo, b.loInc = key, inc
		return b
	}
	if c := bytes.Compare(key, b.lo); c > 0 || (c == 0 && !inc) {
		b.lo, b.loInc = key, inc
	}
	return b
}

// intersectHi tightens b's upper bound to the more restrictive of (current, key); at an equal key an
// exclusive bound wins.
func intersectHi(b keyBound, key []byte, inc bool) keyBound {
	if b.hi == nil {
		b.hi, b.hiInc = key, inc
		return b
	}
	if c := bytes.Compare(key, b.hi); c < 0 || (c == 0 && !inc) {
		b.hi, b.hiInc = key, inc
	}
	return b
}

// boundEmpty reports whether the bound admits no key: lo above hi, or lo == hi with a non-inclusive
// endpoint.
func boundEmpty(b keyBound) bool {
	if b.lo == nil || b.hi == nil {
		return false
	}
	switch bytes.Compare(b.lo, b.hi) {
	case 1:
		return true
	case 0:
		return !(b.loInc && b.hiInc)
	}
	return false
}

// execSelectPlan executes a resolved SELECT against an outer-row environment (outer = the
// enclosing rows, innermost last; nil at top level) and the bound parameters. The execute half
// of the old runSelect: materialize, nested-loop join, WHERE, then aggregate / DISTINCT / window
// + project. The per-row evaluator gets an evalEnv carrying the engine + outer rows, so a
// correlated subquery in any clause re-executes against them (grammar.md §26).
// execStreamingScan executes the streaming primary-key-ordered scan path (spec/design/cost.md §3):
// a single-table, no-blocking-operator query whose output order is already the table's primary-key
// scan order — either no ORDER BY (the LIMIT short-circuit) or an ORDER BY satisfied by PK order
// (plan.pkOrdered, set by orderSatisfiedByPK) — streams scan→filter→project with NO sort, and (when
// there is a LIMIT) stops the scan the instant the LIMIT/OFFSET window is filled, charging
// storage_row_read only for the rows actually read. With no LIMIT it emits every survivor after
// OFFSET (the sort is simply elided — same rows, same cost as the eager/sort path). It is
// cost-equivalent to the eager path EXCEPT that a LIMIT reads (and filters) fewer rows, which is the
// deliberate cost change. page_read is the full block (the bound's node count) — it does not
// short-circuit; only the row reads do. Rows match the eager path exactly: the offset..offset+limit
// slice of the primary-key-ordered filtered rows (which, for a pkOrdered query, IS the ORDER BY's
// result — the stored PK order is the requested order).
// streamingScanEligible reports whether plan is the single-table, no-blocking-operator STREAMING SCAN
// shape (spec/design/cost.md §3, streaming.md §4) — a single relation, no join/aggregate/window, an
// output order the primary-key scan already yields (pkOrdered, or no ORDER BY with a LIMIT
// short-circuit), no index/GIN/GiST bound (those read the full admitted set eagerly), and a real
// table store (not an SRF / CTE / derived source). Both execSelectPlan (which routes to the eager
// execStreamingScan) and tryStreamingQuery (the lazy Query lane) gate on this ONE predicate, so the
// two never drift.
func streamingScanEligible(plan *selectPlan) bool {
	return len(plan.rels) == 1 && len(plan.joins) == 0 &&
		!plan.isAgg && !plan.hasWindow &&
		(plan.phys.pkOrdered || (!plan.distinct && len(plan.order) == 0 && plan.limit != nil)) &&
		!plan.phys.relBounds[0].needsEagerScan() &&
		plan.rels[0].srf == nil &&
		plan.rels[0].cte == nil &&
		plan.rels[0].derived == nil
}

// windowTopNEligible reports whether a plain (non-grouped) window query can serve its LIMIT with a
// TOP-N over the primary-key scan — reading only the first OFFSET+LIMIT rows instead of the whole
// table (spec/design/window.md §5.2 "windowed top-N", cost.md §3). It is the window analog of the
// streaming-scan LIMIT short-circuit above, sound only when every window value at scan position k
// depends solely on rows at positions <= k (a "backward" window over the scan order): then the first
// OFFSET+LIMIT scan rows determine the first OFFSET+LIMIT output rows exactly.
//
// The gate (all must hold): a single base-table full/PK-bounded scan (no join/SRF/CTE/derived, no
// index/GIN/GiST bound — those read the full admitted set), a plain window (`hasWindow && !isAgg`),
// not DISTINCT, a LIMIT present, and an outer ORDER BY the PK scan already satisfies (`pkOrdered`, so
// the scan order IS the output order and no post-window sort reorders rows). No compound
// (materialized) window key (windowKeys) and no general-expression ORDER BY (orderExprs) — those
// append synthetic columns; a bare PK-column window is the shape that streams. Finally EVERY window
// spec must be prefix-safe (windowSpecPrefixSafe). Rows are byte-identical to the eager path; only
// the accrued cost drops (fewer rows scanned/folded), the deliberate cost change (like the streaming
// LIMIT short-circuit — cross-core identical because every core caps at the same OFFSET+LIMIT).
func (db *engine) windowTopNEligible(plan *selectPlan) bool {
	if !plan.hasWindow || plan.isAgg || plan.distinct || plan.limit == nil || !plan.phys.pkOrdered {
		return false
	}
	if len(plan.rels) != 1 || len(plan.joins) != 0 {
		return false
	}
	rel := plan.rels[0]
	if rel.srf != nil || rel.cte != nil || rel.derived != nil {
		return false
	}
	if plan.phys.relBounds[0].needsEagerScan() {
		return false
	}
	if len(plan.windowKeys) != 0 || len(plan.orderExprs) != 0 {
		return false
	}
	table, ok := db.lkpTableScoped(rel.db, rel.tableName)
	if !ok {
		return false
	}
	for i := range plan.windowSpecs {
		if !db.windowSpecPrefixSafe(&plan.windowSpecs[i], plan, table, rel.offset) {
			return false
		}
	}
	return true
}

// windowSpecPrefixSafe reports whether one window function's value at scan position k depends solely
// on rows at positions <= k, so truncating the input to the first OFFSET+LIMIT rows is exact
// (spec/design/window.md §5.2). It requires: no PARTITION BY (the whole scan is one partition, so
// scan order = partition order); a window ORDER BY the PRIMARY KEY satisfies in the SAME direction as
// the outer pkOrdered scan (so the window's "preceding" is the scan's preceding — the sort is a
// no-op); and a backward plan/frame.
//
//   - row_number / rank / dense_rank / lag → backward (position, earlier-peer count, or a look-BACK
//     offset); never depend on later rows or the total partition size.
//   - an aggregate / first_value / last_value / nth_value window → backward iff its FRAME does not
//     look forward (frameBackwardSafe): the frame END must be UNBOUNDED PRECEDING / PRECEDING /
//     CURRENT ROW, and a RANGE/GROUPS CURRENT-ROW end (which spans the current PEER GROUP) is safe
//     only when the ordering key is unique (the full PK) so a peer group is a single row.
//   - percent_rank / cume_dist / ntile depend on the total partition size N; lead looks FORWARD —
//     all rejected.
func (db *engine) windowSpecPrefixSafe(spec *windowSpec, plan *selectPlan, table *catTable, offset int) bool {
	if len(spec.partition) != 0 || len(spec.order) == 0 {
		return false
	}
	ok, rev := db.orderSatisfiedByPK(table, offset, spec.order)
	if !ok || rev != plan.phys.pkReverse {
		return false
	}
	unique := len(spec.order) >= len(table.PKIndices()) // order covers the full (unique) PK ⇒ singleton peer groups
	switch spec.plan {
	case planRowNumber, planRank, planDenseRank, planLag:
		return true
	case planAgg, planFirstValue, planLastValue, planNthValue:
		return frameBackwardSafe(spec.frame, unique)
	default:
		return false // planPercentRank, planCumeDist, planNtile (need N), planLead (looks forward)
	}
}

// frameBackwardSafe reports whether a frame folds only rows at or before the current row in the scan
// order (spec/design/window.md §5.2/§6). The frame END must not look forward; a RANGE/GROUPS
// CURRENT-ROW end spans the current peer group, which pulls in later rows unless the ordering key is
// unique. A ROWS frame uses physical position, so it never expands to peers. The default frame (nil,
// with a window ORDER BY) is RANGE UNBOUNDED PRECEDING TO CURRENT ROW — safe only when the key is
// unique.
func frameBackwardSafe(frame *resolvedFrame, unique bool) bool {
	if frame == nil {
		return unique
	}
	switch frame.end.kind {
	case boundUnboundedPreceding, boundPreceding:
		return true // strictly before the current peer group
	case boundCurrentRow:
		return frame.mode == frameRows || unique // ROWS = the physical row; RANGE/GROUPS = the peer group
	default:
		return false // boundFollowing / boundUnboundedFollowing look forward
	}
}

// orderSatisfiedByPK reports whether a single base relation's ORDER BY is satisfied by its
// PRIMARY-KEY scan order (spec/design/cost.md §3), and in which DIRECTION: it returns
// (satisfied, reverse) where reverse=true means the order is all-DESC over the full PK, served by a
// REVERSE scan, and reverse=false means all-ASC (forward). The direction comes from the first ORDER
// BY key; every PK-prefix key must share it (no mixed ASC/DESC). Two asymmetric coverage rules,
// both grounded in the eager sort being a STABLE sort that breaks ties in input = PK-ascending
// order: forward (ASC) allows a strict PREFIX of the PK (the remaining columns tie-break ascending,
// exactly the input order the stable sort preserves); reverse (DESC) requires the FULL PK
// (len(order) >= len(pk)) because a strict DESC prefix of a composite PK would have the eager sort
// break ties in PK-ascending input order, which a reverse scan inverts — so reverse is restricted
// to the unique full key, where no ties remain.
func (db *engine) orderSatisfiedByPK(table *catTable, offset int, order []orderSlot) (bool, bool) {
	pk := table.PKIndices()
	if len(pk) == 0 {
		return false, false // no PK (synthetic rowid order is not a user-visible column)
	}
	reverse := order[0].descending // direction comes from the first ORDER BY key
	if reverse && len(order) < len(pk) {
		return false, false // a reverse scan needs the full (unique) PK so no ties remain
	}
	m := len(order)
	if len(pk) < m {
		m = len(pk)
	}
	for i := 0; i < m; i++ {
		o := order[i]
		if o.descending != reverse {
			return false, false // every PK-prefix key must share the scan direction (no mixed ASC/DESC)
		}
		if o.idx != offset+pk[i] {
			return false, false // must be the i-th PK column, in key order
		}
		// The ORDER BY key must sort by the SAME order the stored PK key realizes. A raw-byte
		// (C/non-text) key matches a key with no collation; a Full-collated key matches the SAME
		// collation; a Skewed/unresolvable collation never matches (its stored keys are at the
		// file's pinned version, so the scan order would be wrong for the loaded one — §12).
		coll, push := db.keyCollationCtx(table.Columns[pk[i]])
		if !push {
			return false, false // Skewed / unresolvable
		}
		if coll == nil {
			if o.collation != nil {
				return false, false // raw-byte key, but the ORDER BY key carries a collation
			}
		} else {
			if o.collation == nil || o.collation.Name != coll.Name {
				return false, false
			}
		}
	}
	return true, reverse
}

// pkStorageWidth returns the fixed byte width of a table's stored primary key (encodePKKey = the
// bare per-column order-preserving keys concatenated, no NULL tags — a PK is NOT NULL) and true, or
// (0, false) when ANY PK column is variable-width (text/decimal/bytea/interval) or non-scalar
// (range/composite), or the table has no PK. Used by the secondary-index-order scan to peel the PK
// suffix off the END of each index entry key (the "key-suffix skip", cost.md §3) — sound only when
// that suffix is a known fixed length.
func pkStorageWidth(table *catTable) (int, bool) {
	pk := table.PKIndices()
	if len(pk) == 0 {
		return 0, false // a no-PK table keys on a synthetic rowid — not handled this slice
	}
	w := 0
	for _, ci := range pk {
		s, ok := table.Columns[ci].Type.AsScalar()
		if !ok || !s.IsFixedWidth() {
			return 0, false // a non-scalar / variable-width PK suffix is not a fixed peel
		}
		w += s.WidthBytes()
	}
	return w, true
}

// indexOrderPlan is the secondary-index-order plan: walk a B-tree index in key order to satisfy an
// ORDER BY without a sort, point-looking-up each row by its primary key (cost.md §3).
type indexOrderPlan struct {
	nameKey string // the index store's key — the lowercased index name
	pkWidth int    // the fixed PK-suffix byte width to peel off the END of each index entry key
}

// orderSatisfiedByIndex reports whether a single base relation's ORDER BY is satisfied by walking one
// of its B-tree SECONDARY indexes in key order (cost.md §3 "secondary-index order"), and which index.
// The index store holds its entries in (indexed columns, storage key) order, so a forward walk
// delivers rows in ORDER BY <indexed columns> ASC NULLS LAST order, ties broken by the PK — exactly
// the eager stable sort's tie-break. Returns non-nil iff the ORDER BY keys are EXACTLY a B-tree
// index's columns (same count, same columns in key order), each ASC with default NULLS LAST (the
// index stores NULL as 0x01 after a present 0x00 → NULLS LAST; an explicit NULLS FIRST does not
// match) and sorting by the column's stored key collation (Skewed/unresolvable → refuse, §12), AND
// the table's PK is fixed-width. The exact-match requirement is load-bearing: a strict prefix of a
// multi-column index would tie-break by the remaining index columns, not the PK.
func (db *engine) orderSatisfiedByIndex(table *catTable, offset int, order []orderSlot) *indexOrderPlan {
	pkWidth, ok := pkStorageWidth(table)
	if !ok {
		return nil
	}
	for _, idx := range table.Indexes {
		if idx.Kind != indexBtree {
			continue // only an ordered B-tree realizes the column order (GIN/GiST do not)
		}
		// A PARTIAL index is not used for ORDER-BY skip-sort this slice (indexes.md §9): it holds
		// only its qualifying rows, so walking it would drop rows unless the query implies the
		// predicate — that gate lives only on the access-predicate bound. Stays non-partial (a
		// follow-on); falling through leaves a correct full-scan + sort.
		if idx.Predicate != nil {
			continue
		}
		// ORDER-BY skip-sort is column-only this slice (matching ORDER BY against an expression
		// index key is a follow-on — indexes.md §5).
		cols := idx.columnOrdinals()
		if cols == nil {
			continue
		}
		if len(order) != len(cols) {
			continue // the ORDER BY must be EXACTLY the index columns (see the doc — tie-break)
		}
		matches := true
		for i, o := range order {
			if o.descending || o.nullsFirst {
				matches = false // ASC + NULLS LAST only — the order a forward index walk realizes
				break
			}
			if o.idx != offset+cols[i] {
				matches = false
				break
			}
			coll, push := db.keyCollationCtx(table.Columns[cols[i]])
			if !push { // Skewed / unresolvable — never walked for order (§12)
				matches = false
				break
			}
			if coll == nil {
				if o.collation != nil {
					matches = false
					break
				}
			} else if o.collation == nil || o.collation.Name != coll.Name {
				matches = false
				break
			}
		}
		if matches {
			return &indexOrderPlan{nameKey: strings.ToLower(idx.Name), pkWidth: pkWidth}
		}
	}
	return nil
}
