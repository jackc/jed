package jed

import (
	"bytes"
	"cmp"
	"math"
	"slices"
	"sort"
)

// Access-path MECHANISMS — turning a WHERE filter into a bounded scan (spec/design/cost.md §3;
// planner.md §4 — the machinery the optimize.go rules call, not the rules themselves: it also
// serves UPDATE/DELETE planning and exec-time eligibility). This file holds the row-source scan
// interface (rowSource/scanSource), the access-plan shapes
// (pkBoundPlan/scanBound/indexBoundPlan/gistBoundPlan/ginBoundPlan/interval-set plans), the predicate
// analysis that detects a point lookup / range / index / GIN / GiST bound from a filter
// (detectPKBound/inventoryScanCandidates/detectIntervalSet/buildGinBoundForIndex/
// buildGistBoundForIndex/buildIndexAccessPredicate),
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

// pkEqCol is one member of the maximal equality prefix of a primary-key tuple.
type pkEqCol struct {
	name    string
	colType scalarType
	coll    *Collation
	srcs    []*rExpr
	ranges  []boundTerm
}

// pkBoundPlan is the plan-time result of PK tuple analysis: a maximal equality prefix plus an
// optional range on the next member. The concrete storage-key range is built per execution.
type pkBoundPlan struct {
	eqCols      []pkEqCol
	rangeName   string
	rangeType   scalarType
	rangeColl   *Collation
	rangeTerms  []boundTerm
	memberCount int
}

// scanBound is a per-relation scan bound (cost.md §3): a primary-key range, a
// secondary-index equality (spec/design/indexes.md §5), a GIN-bounded scan over an
// array column (spec/design/gin.md §6), a GiST-bounded scan, or a canonical interval set — exactly
// one field is set. Candidate inventory and consumer selection are deliberately separate; this
// union remains the executor-facing shape of the selected candidate.
type scanBound struct {
	pk       *pkBoundPlan
	index    *indexBoundPlan
	gin      *ginBoundPlan
	gist     *gistBoundPlan
	pkSet    *pkKeySetPlan
	indexSet *indexKeySetPlan
}

// scanCandidateKind is the canonical access-path rank from estimator.toml. Keep the declaration
// order byte-identical to that shared fact: P4 will compare estimated cost first, then this rank.
// P3 uses it only to make the complete inventory deterministic; legacy selection remains separate.
type scanCandidateKind uint8

const (
	scanCandidatePK scanCandidateKind = iota
	scanCandidateBtree
	scanCandidateGist
	scanCandidateGin
	scanCandidatePKInterval
	scanCandidateIndexInterval
	scanCandidateFull
)

// scanCandidateIdentity is a candidate's canonical, collision-free physical identity. indexName is
// the exact catalog name for an index-bearing path and empty for PK/PK-interval/full paths.
// Index names are unique within the relation namespace, so (kind, indexName) is total for an
// inventory.
type scanCandidateIdentity struct {
	kind      scanCandidateKind
	indexName string
}

func (id scanCandidateIdentity) String() string {
	switch id.kind {
	case scanCandidatePK:
		return "pk"
	case scanCandidateBtree:
		return "btree:" + id.indexName
	case scanCandidateGist:
		return "gist:" + id.indexName
	case scanCandidateGin:
		return "gin:" + id.indexName
	case scanCandidatePKInterval:
		return "pk_interval"
	case scanCandidateIndexInterval:
		return "index_interval:" + id.indexName
	case scanCandidateFull:
		return "full"
	default:
		panic("unknown scan candidate kind")
	}
}

type scanOrderKind uint8

const (
	// scanOrderStorageKey is table-storage-key order. Full/PK/PK-set scans walk the table tree in
	// this order; GIN/GiST candidate gathers normalize to it before table fetch. The current executor
	// can walk it both forward and reverse.
	scanOrderStorageKey scanOrderKind = iota
	// scanOrderIndexKey is one ordered B-tree's key order. The current ordered-index path walks only
	// forward; indexName identifies exactly which ORDER BY capability it can satisfy.
	scanOrderIndexKey
)

// scanOrderCapability makes an access path's observable row-order ability explicit rather than
// re-deriving it from the ScanBound union at each optimizer consumer.
type scanOrderCapability struct {
	kind       scanOrderKind
	indexName  string
	reversible bool
}

// scanCandidate is one legal base-relation access path. bound is nil only for the explicit full-scan
// candidate. residual is always the complete WHERE (nil only when there is no WHERE): every access
// predicate is a narrowing superset and execution must retain the full filter recheck.
type scanCandidate struct {
	identity  scanCandidateIdentity
	bound     *scanBound
	scanOrder scanOrderCapability
	residual  *rExpr
}

func storageOrderCandidate(kind scanCandidateKind, name string, bound *scanBound, filter *rExpr) scanCandidate {
	return scanCandidate{
		identity:  scanCandidateIdentity{kind: kind, indexName: name},
		bound:     bound,
		scanOrder: scanOrderCapability{kind: scanOrderStorageKey, reversible: true},
		residual:  filter,
	}
}

func indexOrderCandidate(kind scanCandidateKind, name string, bound *scanBound, filter *rExpr) scanCandidate {
	return scanCandidate{
		identity:  scanCandidateIdentity{kind: kind, indexName: name},
		bound:     bound,
		scanOrder: scanOrderCapability{kind: scanOrderIndexKey, indexName: name},
		residual:  filter,
	}
}

// mutationScanPlan is the small physical plan shared by UPDATE/DELETE execution and DML EXPLAIN.
// The resolved filter remains the residual predicate; bound is only the chosen candidate superset.
// scope carries the target database qualifier so a full scan continues through the scoped store
// funnel (attachments deliberately have no bound this slice).
type mutationScanPlan struct {
	bound  *scanBound
	filter *rExpr
	scope  *string
	// contradiction is the stage-2 proof (planner.md §3.1) that the WHERE is never TRUE: the bound is
	// still selected (EXPLAIN renders it), but execution reads nothing.
	contradiction bool
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
// GiST) and for a canonical interval set (pkSet / indexSet); false for a nil bound or a plain PK
// contiguous bound (which every fast path
// handles via a single buildKeyBound). Every single-table fast-path gate consults this so the
// interval-set bounds are interpreted in exactly ONE place (materializeRel), never silently dropped to
// a full scan by a fast path that only understands `pk`. Nil-safe (a nil bound is not eager).
func (sb *scanBound) needsEagerScan() bool {
	return sb != nil && (sb.index != nil || sb.gin != nil || sb.gist != nil || sb.pkSet != nil || sb.indexSet != nil)
}

// intervalSpec is one OR disjunct represented as the conjunction of its bound terms. It becomes
// one logical key interval at execution, after params/outer sources are known. An array spec
// (array != nil, no terms) is a `key = ANY(array)` disjunct: at execution it becomes one point per
// non-NULL element of the array's value.
type intervalSpec struct {
	terms []boundTerm
	array *rExpr
}

// pkKeySetPlan is the generalized OR/IN interval-set plan over a single-column PK. Each spec is a
// point or range disjunct; clip terms are co-present top-level AND bounds on the same key. Execution
// encodes, clips, sorts, and merges them into canonical disjoint intervals.
//
// A composite PK instead carries tuple (cost.md §3 "Composite primary keys"): the consumed members
// in key order, every one but possibly the last a point member. pkType/coll/specs/clip are then
// unused.
type pkKeySetPlan struct {
	pkType scalarType
	coll   *Collation
	specs  []intervalSpec
	clip   []boundTerm

	tuple       []tupleMember
	memberCount int
	allFixed    bool
}

// tupleMember is one consumed member of a composite-PK tuple interval set. specs is the member's
// first reducing OR/IN/ANY conjunct (nil when it has none); clip is every direct comparison on it.
// point marks a point member (a direct equality, or a set of pure equalities/arrays); a non-point
// member is the final member, whose canonical intervals map under each prefix.
type tupleMember struct {
	name    string
	column  int
	colType scalarType
	coll    *Collation
	specs   []intervalSpec
	clip    []boundTerm
	point   bool
}

// maxKeySetProbes caps the running product of a composite-PK tuple set's member counts
// (cost.md §3): a later member with more than one point that would exceed it is not consumed, nor
// any member after it, so the unmetered cartesian expansion stays input-linear.
const maxKeySetProbes = 65536

// intervals resolves the set's canonical storage-key intervals for one execution.
func (ks *pkKeySetPlan) intervals(params []Value, outer []storedRow, left storedRow) []keyBound {
	if ks.tuple != nil {
		return tupleIntervalSet(ks, params, outer, left)
	}
	return canonicalIntervalSet(ks.pkType, ks.specs, ks.clip, params, outer, ks.coll, left)
}

// indexKeySetPlan is the pkKeySetPlan analog over a B-tree secondary index whose keys are plain
// columns (indexes.md §5; cost.md §3 "Multi-column secondary indexes"): tuple is the consumed key
// columns in key order, walked as a composite-PK tuple set's members, and slots is every key
// column's self-delimiting slot form (indexes.md §5.1) — the per-entry key-suffix skip. A tuple of one member is the leading-column
// interval set. allFixed reports that every key column is fixed-width, so the probes merge as the
// PK's do.
type indexKeySetPlan struct {
	nameKey string
	tuple   []tupleMember
	slots   []keySlot
	// orders is every key column's sort order (indexes.md §11), parallel to slots: a member's
	// points are its §2.16 present slots.
	orders   []keyOrder
	allFixed bool
}

// indexProbe is one canonical index interval with its row-key suffix recovery: the known prefix
// byte length skipped first, then the self-delimiting member slots after it (indexes.md §5.1).
type indexProbe struct {
	bound     keyBound
	prefixLen int
	suffix    []keySlot
}

// probes resolves the set's canonical index-key intervals for one execution, in index-key order.
func (ks *indexKeySetPlan) probes(params []Value, outer []storedRow, left storedRow) []indexProbe {
	prefixes, final, consumed, hasFinal := expandTuplePrefixes(ks.tuple, ks.orders, params, outer, left)
	var out []indexProbe
	for _, p := range prefixes {
		if !hasFinal {
			out = append(out, indexProbe{bound: keyBound{lo: p, loInc: true, hi: prefixSuccessor(p)}, prefixLen: len(p), suffix: ks.slots[consumed:]})
			continue
		}
		for _, iv := range final {
			phys := indexLogicalInterval(iv, ks.orders[consumed-1])
			b := prefixedInterval(p, phys)
			if !boundEmpty(b) {
				out = append(out, indexProbe{bound: b, prefixLen: len(p), suffix: ks.slots[consumed-1:]})
			}
		}
	}
	// The probes are disjoint; a DESC final member emits its intervals in reverse physical order, so
	// order them by their lower endpoint — the index-key order the walk must follow.
	sort.SliceStable(out, func(i, j int) bool { return bytes.Compare(out[i].bound.lo, out[j].bound.lo) < 0 })
	if !ks.allFixed || len(out) < 2 {
		return out
	}
	// Every slot is fixed-width, so the merged intervals recover their suffix by width from the key's
	// start.
	bounds := make([]keyBound, len(out))
	for i, pr := range out {
		bounds[i] = pr.bound
	}
	merged := mergeKeyIntervals(bounds, true)
	out = out[:0]
	for _, b := range merged {
		out = append(out, indexProbe{bound: b, suffix: ks.slots})
	}
	return out
}

func intervalPlanHasRange(specs []intervalSpec, clip []boundTerm) bool {
	for _, spec := range specs {
		for _, t := range spec.terms {
			if t.op != opEq {
				return true
			}
		}
	}
	for _, t := range clip {
		if t.op != opEq {
			return true
		}
	}
	return false
}

// gistBoundPlan is the plan-time result for one eligible GiST index (spec/design/gist.md §5): its
// descent strategy and the column's global scope index. The inventory owns one plan per eligible
// index; the selector chooses among them. Like ginBoundPlan, the constant query operand is NOT stored
// (re-found in plan.filter at exec time by gistMatch). No element type is carried — the gather
// descends the resident R-tree (gist.md §4.1), whose bounds are already decoded.
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
	// The jsonb_ops strategies (gin.md §2.1), matched only on a jsonb GIN column: `col @> Q` (Q <@ col
	// resolves to the same node — the column is the CONTAINING operand; Q's index terms, mode ALL),
	// `col ? k` (one key term), `col ?| K` (the non-NULL keys' terms, mode ANY), `col ?& K` (the
	// non-NULL keys' terms, mode ALL). The recovered query operand is Q / k / K.
	ginJsonbContains
	ginJsonbExists
	ginJsonbExistsAny
	ginJsonbExistsAll
	// ginJsonPathExists / ginJsonPathMatch are jsonb_ops `col @? P` / `col @@ P` (gin.md §2.2): the
	// AND/OR term tree PostgreSQL extracts from the jsonpath; nothing extractable falls back to the
	// full scan. The recovered query operand is the jsonpath P.
	ginJsonPathExists
	ginJsonPathMatch
)

// ginBoundPlan is the plan-time result for one eligible GIN index (spec/design/gin.md §6): its array
// ELEMENT type (for encode(term) — the term bytes), operator strategy, and column's global scope
// index. The inventory owns one plan per eligible index; the selector chooses among them. The
// constant query Q is NOT stored; it is re-found in plan.filter at exec time by ginMatch and
// evaluated there.
type ginBoundPlan struct {
	nameKey   string
	elemType  scalarType
	strategy  ginStrategy
	colGlobal int
	// jsonb is true for a jsonb column (the jsonb_ops opclass, gin.md §2.1; elemType unused), false
	// for an array column (array_ops). It selects which strategies ginMatch admits.
	jsonb bool
	// orArms is a disjunctive bound's per-arm strategies (gin.md §6.3): every arm of an OR conjunct
	// (an IN list desugars to one) whose every disjunct holds an accelerable conjunct on this
	// column, in source order. Empty for the ordinary single-conjunct bound; non-empty, strategy is
	// the first arm's.
	orArms []ginStrategy
	// tag/tagged is a multi-column GIN index's column tag for the bound's column (gin.md §4.4),
	// prefixed to every query term; untagged for a single-column index.
	tag    byte
	tagged bool
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
	// order is the key element's sort order (indexes.md §11): the probe slot is its §2.16 present
	// slot.
	order keyOrder
}

// indexBoundPlan is the plan-time result for one eligible ordered index (indexes.md §5.1): a maximal
// EQUALITY PREFIX on the leading key columns (eqCols) plus an OPTIONAL RANGE on the next column
// (rangeTerms / rangeType). The inventory owns one plan per eligible index; the selector chooses
// among them. At exec time buildIndexBound turns these into a concrete index-key
// range: the equality prefix bytes P = concatenated present slots, then the range (if any)
// intersected relative to P. suffix holds the slot forms of the index key elements AFTER the
// equality prefix (keys[len(eqCols):]) — the range element (if any) plus every trailing element —
// each skipped by its self-delimiting encoding so an admitted entry's row-key suffix is recovered
// past P. An empty eqCols with no range is a FULL partial-index scan (indexes.md §9.2).
type indexBoundPlan struct {
	nameKey   string // the index store's key — the exact index name
	eqCols    []indexEqCol
	rangeType scalarType // the range column's type (meaningful iff rangeTerms != nil)
	// rangeColl is the range column's key collation: a collated text column encodes its endpoints
	// as the column collation's UCA sort key, the stored key form (encoding.md §2.12).
	rangeColl  *Collation
	rangeTerms []boundTerm // range conjuncts on the column after the equality prefix (nil ⇒ none)
	// rangeOrder is the range element's sort order (indexes.md §11): a DESC element swaps the byte
	// endpoints.
	rangeOrder keyOrder
	suffix     []keySlot
}

// buildIndexAccessPredicate constructs an index access predicate for idx over rel (indexes.md
// §5.1): a maximal EQUALITY PREFIX on the leading key columns plus an OPTIONAL RANGE on the next
// column. It walks the index's key columns in key order against the WHERE AND-chain, consuming a
// column with an agreed equality conjunct into the prefix and stopping at the first column that
// has no equality (taking its range conjuncts, if any, as the trailing range). Returns nil for a
// non-B-tree index, a Skewed collated bound column (whose stored keys are at the file's pinned
// version — collation.md §12), no bound at all, or an ineligible suffix (a column after the
// equality prefix that is not a fixed-width scalar — the width-based key-suffix skip needs it).
// siblingColumns opens the index-nested-loop door by admitting a bare sibling reColumn in the
// selected physical-left relation's global slot interval; nil is an ordinary bound.
func (db *engine) buildIndexAccessPredicate(filter *rExpr, rel scopeRel, idx indexDef, siblingColumns columnRanges) *indexBoundPlan {
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
	// (A partial index's predicate needs no NULL-rejection here: every bound term below is a strict
	// comparison on this relation's key, so its WHERE conjunct rejects a NULL-extended row.)
	var rangeType scalarType
	var rangeColl *Collation
	var rangeTerms []boundTerm
	var rangeOrder keyOrder
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
			if t, ok := asBoundTerm(e, matcher, ty, colColl, siblingColumns); ok {
				if t.op == opEq {
					eqs = append(eqs, t.src)
				} else {
					ranges = append(ranges, t)
				}
			}
		}
		walk(filter)
		if len(eqs) > 0 {
			eqCols = append(eqCols, indexEqCol{colType: ty, coll: coll, srcs: eqs, order: rindex.keyOrder(i)})
			continue // extend the equality prefix
		}
		if len(ranges) > 0 {
			rangeType = ty
			rangeColl = coll
			rangeTerms = ranges
			rangeOrder = rindex.keyOrder(i)
		}
		break // first non-equality element ends the prefix (with or without a trailing range)
	}
	if len(eqCols) == 0 && rangeTerms == nil {
		return nil // nothing bound
	}
	suffix, ok := indexSuffixSlots(rel, rindex, len(eqCols))
	if !ok {
		return nil
	}
	return &indexBoundPlan{
		nameKey: idx.Name, eqCols: eqCols,
		rangeType: rangeType, rangeColl: rangeColl, rangeTerms: rangeTerms, rangeOrder: rangeOrder, suffix: suffix,
	}
}

// indexSuffixSlots is the slot forms of the resolved index's key elements from `from` onward
// (indexes.md §5.1): each is skipped past the known equality prefix to recover an entry's storage
// key, so each must be a scalar with a key encoding (a column's type, or an expression's result
// type). ok=false when one is a container (range/array/composite) element.
func indexSuffixSlots(rel scopeRel, rindex resolvedIndex, from int) ([]keySlot, bool) {
	suffix := make([]keySlot, 0, max(len(rindex.Keys)-from, 0))
	for j, key := range rindex.Keys[from:] {
		var slot keySlot
		if key.Expr == nil {
			col := rel.table.Columns[key.Col]
			s, ok := col.Type.AsScalar()
			if !ok {
				return nil, false
			}
			if slot, ok = keySlotForScalar(s, col.Collation != ""); !ok {
				return nil, false
			}
		} else {
			s, ok := key.Ty.AsScalar()
			if !ok {
				return nil, false
			}
			if slot, ok = keySlotForScalar(s, key.Coll != nil); !ok {
				return nil, false
			}
		}
		// The slot reads the element's own NULL tag and (DESC) inverted body (indexes.md §11).
		suffix = append(suffix, slot.withOrder(rindex.keyOrder(from+j)))
	}
	return suffix, true
}

// buildPartialIndexFullScan is a FULL partial-index scan (indexes.md §9.2): a B-tree PARTIAL index
// whose predicate the WHERE implies (§9.1) is a complete source of the query's rows even without an
// equality/range access predicate, so it is a candidate that walks the whole (narrow) index. Offered
// only for a single-relation scan (a join's relation never NULL-extends here) and only when no
// ordinary access predicate already bounds this index. Every key element must be skippable (§5.1).
func (db *engine) buildPartialIndexFullScan(filter *rExpr, rel scopeRel, idx indexDef) *indexBoundPlan {
	if idx.Kind != indexBtree || idx.Predicate == nil {
		return nil
	}
	rindex, err := db.resolveIndex(rel.table, idx)
	if err != nil || rindex.Predicate == nil {
		return nil
	}
	if !filterImpliesPredicate(filter, rindex.Predicate, rel.offset) {
		return nil
	}
	// A skewed collated key element's stored order is not the loaded collation's; never walk it
	// (the read-safety rule, collation.md §12).
	for _, key := range rindex.Keys {
		if key.Expr == nil {
			if _, push := db.keyCollationCtx(rel.table.Columns[key.Col]); !push {
				return nil
			}
		}
	}
	suffix, ok := indexSuffixSlots(rel, rindex, 0)
	if !ok {
		return nil
	}
	return &indexBoundPlan{nameKey: idx.Name, suffix: suffix}
}

// inventorySingleRelationCandidates is the single-relation access inventory (estimator.md §9.1,
// §9.3): inventoryScanCandidates plus a FULL partial-index scan for every partial index whose
// predicate the WHERE implies and that has no ordinary access-predicate candidate (indexes.md §9.2),
// in canonical kind/name order.
func inventorySingleRelationCandidates(filter *rExpr, rel scopeRel, db *engine) []scanCandidate {
	candidates := inventoryScanCandidates(filter, rel, db)
	if filter == nil || rel.isAttachment() {
		return candidates
	}
	added := false
	for _, idx := range rel.table.Indexes {
		hasBound := false
		for _, c := range candidates {
			if c.identity.kind == scanCandidateBtree && c.identity.indexName == idx.Name {
				hasBound = true
				break
			}
		}
		if hasBound {
			continue
		}
		if ib := db.buildPartialIndexFullScan(filter, rel, idx); ib != nil {
			candidates = append(candidates, indexOrderCandidate(scanCandidateBtree, ib.nameKey, &scanBound{index: ib}, filter))
			added = true
		}
	}
	if added {
		sortScanCandidates(candidates)
	}
	return candidates
}

func sortScanCandidates(candidates []scanCandidate) {
	sort.SliceStable(candidates, func(i, j int) bool {
		a, b := candidates[i].identity, candidates[j].identity
		if a.kind != b.kind {
			return a.kind < b.kind
		}
		return bytes.Compare([]byte(a.indexName), []byte(b.indexName)) < 0
	})
}

// detectScanBound picks one SELECT relation's scan bound (cost.md §3; indexes.md §5). It is the
// fixed-policy wrapper over the shared inventory + behavior-neutral legacy selector, used by the
// SELECT barrier boundaries that have not moved to cost selection.
func detectScanBound(filter *rExpr, rel scopeRel, db *engine) *scanBound {
	return selectLegacyScanCandidate(inventoryScanCandidates(filter, rel, db))
}

// inventoryScanCandidates enumerates EVERY legal base access path in estimator.toml's canonical
// rank/name order. It never selects. A host-attached relation has only the full candidate this slice
// because bounded execution still resolves its index stores through the unscoped funnel.
func inventoryScanCandidates(filter *rExpr, rel scopeRel, db *engine) []scanCandidate {
	full := storageOrderCandidate(scanCandidateFull, "", nil, filter)
	if rel.isAttachment() || filter == nil {
		return []scanCandidate{full}
	}
	candidates := make([]scanCandidate, 0, 2+len(rel.table.Indexes)*2)
	if bp := db.detectPKBound([]*rExpr{filter}, rel, nil); bp != nil {
		candidates = append(candidates, storageOrderCandidate(scanCandidatePK, "", &scanBound{pk: bp}, filter))
	}
	// Do not trust catalog/container iteration for identity order. The final sort applies the shared
	// kind rank followed by raw UTF-8 bytes of the exact index name.
	for _, idx := range rel.table.Indexes {
		if ib := db.buildIndexAccessPredicate(filter, rel, idx, nil); ib != nil {
			candidates = append(candidates, indexOrderCandidate(scanCandidateBtree, ib.nameKey, &scanBound{index: ib}, filter))
		}
	}
	for _, idx := range rel.table.Indexes {
		if gb := buildGistBoundForIndex(filter, idx, rel.table.Columns, rel.offset); gb != nil {
			candidates = append(candidates, storageOrderCandidate(scanCandidateGist, gb.nameKey, &scanBound{gist: gb}, filter))
		}
	}
	for _, idx := range rel.table.Indexes {
		if gb := buildGinBoundForIndex(filter, idx, rel.table.Columns, rel.offset); gb != nil {
			candidates = append(candidates, storageOrderCandidate(scanCandidateGin, gb.nameKey, &scanBound{gin: gb}, filter))
		}
	}
	pkIntervals := db.detectTupleIntervalSet(filter, rel)
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				if specs, clip := detectIntervalSet(filter, rel.offset+pkLocal, sty, coll); specs != nil {
					pkIntervals = &pkKeySetPlan{pkType: sty, coll: coll, specs: specs, clip: clip}
				}
			}
		}
	}
	if pkIntervals != nil {
		candidates = append(candidates, storageOrderCandidate(scanCandidatePKInterval, "", &scanBound{pkSet: pkIntervals}, filter))
	}
	for _, idx := range rel.table.Indexes {
		if is := db.buildIndexIntervalSetPlan(filter, rel, idx); is != nil {
			candidates = append(candidates, indexOrderCandidate(scanCandidateIndexInterval, is.nameKey, &scanBound{indexSet: is}, filter))
		}
	}
	candidates = append(candidates, full)
	sortScanCandidates(candidates)
	return candidates
}

func firstScanCandidate(candidates []scanCandidate, kind scanCandidateKind) *scanCandidate {
	for i := range candidates {
		if candidates[i].identity.kind == kind {
			return &candidates[i]
		}
	}
	return nil
}

func namedScanCandidate(candidates []scanCandidate, kind scanCandidateKind, name string) *scanCandidate {
	for i := range candidates {
		if candidates[i].identity.kind == kind && candidates[i].identity.indexName == name {
			return &candidates[i]
		}
	}
	return nil
}

// selectLegacyScanCandidate reproduces the pre-P3 fixed SELECT policy exactly for the barrier
// shapes that have not moved to cost selection. Its order deliberately is NOT the inventory's
// canonical cost-tie order in one case: a clipped same-key interval set replaces its broader
// contiguous PK/index bound. Returning nil selects the explicit full candidate while preserving the
// physical plan's existing nil spelling.
func selectLegacyScanCandidate(candidates []scanCandidate) *scanBound {
	// A composite-PK tuple set is always contained in the contiguous PK bound, so it replaces it.
	if c := firstScanCandidate(candidates, scanCandidatePKInterval); c != nil && (len(c.bound.pkSet.clip) > 0 || c.bound.pkSet.tuple != nil) {
		return c.bound
	}
	if c := firstScanCandidate(candidates, scanCandidatePK); c != nil {
		return c.bound
	}
	for _, c := range candidates {
		if c.identity.kind != scanCandidateBtree {
			continue
		}
		// A same-index set with a clipped leading set, or one consuming more than the leading column,
		// is contained in the contiguous bound, so it replaces it.
		if set := namedScanCandidate(candidates, scanCandidateIndexInterval, c.identity.indexName); set != nil && (len(set.bound.indexSet.tuple[0].clip) > 0 || len(set.bound.indexSet.tuple) > 1) {
			return set.bound
		}
		return c.bound
	}
	if c := firstScanCandidate(candidates, scanCandidateGist); c != nil {
		return c.bound
	}
	if c := firstScanCandidate(candidates, scanCandidateGin); c != nil {
		return c.bound
	}
	if c := firstScanCandidate(candidates, scanCandidatePKInterval); c != nil {
		return c.bound
	}
	if c := firstScanCandidate(candidates, scanCandidateIndexInterval); c != nil {
		return c.bound
	}
	return nil
}

// selectCostedScanCandidate selects the lowest base-access estimate across the complete P3
// inventory. P6b's whole-pipeline rule subsequently recomputes the eligible single-relation winner
// after composing ordering and LIMIT/OFFSET; this helper supplies the provisional rule-1 choice.
// Candidates are already in canonical P0 tie order, so retaining the first exact-cost winner applies
// the kind/name tie-break without host iteration.
func selectCostedScanCandidate(candidates []scanCandidate, estimates []candidateEstimate, legacy *scanBound) *scanBound {
	if len(candidates) == 0 || len(candidates) != len(estimates) {
		return legacy
	}
	winner := -1
	for i := range candidates {
		if winner == -1 || estimates[i].cost < estimates[winner].cost {
			winner = i
		}
	}
	if winner == -1 {
		return legacy
	}
	return candidates[winner].bound
}

// scanBoundHasStorageOrder reports the scan-order capability used by ORDER BY/scan composition.
// Full, PK, PK-interval, and normalized GIN/GiST candidates emit table storage-key order. Ordered
// B-tree candidates and index interval sets emit their named index order instead.
func scanBoundHasStorageOrder(bound *scanBound) bool {
	return bound == nil || bound.pk != nil || bound.pkSet != nil || bound.gin != nil || bound.gist != nil
}

func (db *engine) buildIndexIntervalSetPlan(filter *rExpr, rel scopeRel, idx indexDef) *indexKeySetPlan {
	if filter == nil {
		return nil
	}
	// A PARTIAL index serves the set only when the WHERE implies its predicate (indexes.md §9.1);
	// the set's own disjunction is a NULL-rejecting conjunct on this relation's key.
	partialOK := idx.Predicate == nil || db.partialIndexImplied(filter, rel.table, rel.offset, idx)
	return db.buildIndexIntervalSetPlanConjuncts(flattenConjuncts(filter), rel, idx, nil, partialOK)
}

// buildIndexIntervalSetPlanConjuncts is buildIndexIntervalSetPlan over an AND-chain's conjuncts;
// siblings admits a sibling column as a member's source (an index-nested-loop inner). partialOK is the
// caller's proof that a PARTIAL index's predicate is implied (indexes.md §9.1, §9.3).
func (db *engine) buildIndexIntervalSetPlanConjuncts(conjuncts []*rExpr, rel scopeRel, idx indexDef, siblings columnRanges, partialOK bool) *indexKeySetPlan {
	if idx.Kind != indexBtree || (idx.Predicate != nil && !partialOK) {
		return nil
	}
	cols := idx.columnOrdinals()
	if cols == nil {
		return nil
	}
	members, hasSet := db.walkTupleMembers(conjuncts, rel, cols, siblings)
	if !hasSet {
		return nil
	}
	// slotOf is a key column's self-delimiting slot form (indexes.md §5.1): every scalar key type,
	// collated text included, can be stepped over without knowing its value.
	slotOf := func(c int) (keySlot, bool) {
		s, ok := rel.table.Columns[c].Type.AsScalar()
		if !ok {
			return keySlot{}, false
		}
		return keySlotForScalar(s, rel.table.Columns[c].Collation != "")
	}
	skippable := func(c int) bool {
		_, ok := slotOf(c)
		return ok
	}
	// A leading range, or a leading membership subquery (which widens to the whole index if it is
	// read before its fold), leaves the leading slot to be stepped over.
	if lead := members[0]; !skippable(cols[0]) && (intervalPlanHasRange(lead.specs, lead.clip) || specsHaveSubquery(lead.specs)) {
		return nil
	}
	// Suffix eligibility (cost.md §3 "Multi-column secondary indexes"): every key column from the
	// first member after the leading one that may go unconsumed or be a range onward is skipped.
	t := len(members)
	for i := 1; i < len(members); i++ {
		if members[i].specs != nil || !members[i].point {
			t = i
			break
		}
	}
	slots := make([]keySlot, len(cols))
	orders := make([]keyOrder, len(cols))
	allFixed := true
	for i, c := range cols {
		slot, skip := slotOf(c)
		if i >= t && !skip {
			return nil
		}
		s, ok := rel.table.Columns[c].Type.AsScalar()
		allFixed = allFixed && ok && s.IsFixedWidth()
		slots[i] = slot.withOrder(idx.keyOrder(i))
		orders[i] = idx.keyOrder(i)
	}
	return &indexKeySetPlan{nameKey: idx.Name, tuple: members, slots: slots, orders: orders, allFixed: allFixed}
}

// planMutationScan selects an UPDATE/DELETE target access path by cost (estimator.md §9.3): every
// legal candidate in the shared inventory is estimated over the same visible snapshot the scan reads,
// and the lowest scan-plus-residual estimate wins, the first in canonical kind/name order on an
// exact tie. Mutation-only work (assignments, checks, writes, RETURNING) is per affected row and
// identical across candidates, so it cannot change the winner and is not added. Execution calls this
// after uncorrelated filter folding; EXPLAIN calls it on its resolved (unfolded) filter. contradiction
// is the caller's stage-2 proof over the unfolded WHERE (whereContradicts), so both agree.
func (db *engine) planMutationScan(scope *string, table *catTable, filter *rExpr, contradiction bool) mutationScanPlan {
	plan := mutationScanPlan{filter: filter, scope: scope, contradiction: contradiction}
	if filter == nil {
		return plan
	}
	rel := scopeRel{label: table.Name, table: table, offset: 0, db: scope}
	candidates := inventorySingleRelationCandidates(filter, rel, db)
	plan.bound = selectCostedScanCandidate(candidates, db.estimateScanCandidates(candidates, rel, false), selectLegacyScanCandidate(candidates))
	return plan
}

// detectIntervalSet finds the first top-level AND conjunct that is a pure OR of intervals on one
// key. A leaf is one comparison or an AND of comparisons (BETWEEN's resolved shape). Other direct
// top-level comparisons on the same key become a global clip, implementing IN/OR ∩ range.
func detectIntervalSet(filter *rExpr, keyIdx int, keyType scalarType, coll *Collation) (specs []intervalSpec, clip []boundTerm) {
	if filter == nil {
		return nil, nil
	}
	return detectIntervalSetConjuncts(flattenConjuncts(filter), keyIdx, keyType, coll, nil)
}

// detectIntervalSetConjuncts is detectIntervalSet over an AND-chain's conjuncts; siblings admits a
// sibling column as a source (an index-nested-loop inner, cost.md §3 "JOIN").
func detectIntervalSetConjuncts(conjuncts []*rExpr, keyIdx int, keyType scalarType, coll *Collation, siblings columnRanges) (specs []intervalSpec, clip []boundTerm) {
	colColl := ""
	if coll != nil {
		colColl = coll.Name
	}
	found := -1
	for i, e := range conjuncts {
		if e.kind != reOr && e.kind != reQuantified && e.kind != reSubquery && e.kind != reInValues {
			continue
		}
		if reduced, ok := reduceIntervalUnion(e, keyIdx, keyType, colColl, siblings); ok {
			specs, found = reduced, i
			break
		}
	}
	if found < 0 {
		return nil, nil
	}
	for i, e := range conjuncts {
		if i == found {
			continue
		}
		if t, ok := asBoundTerm(e, columnMatch(keyIdx), keyType, colColl, siblings); ok {
			clip = append(clip, t)
		}
	}
	return specs, clip
}

// detectTupleIntervalSet builds a composite-PK tuple interval set (cost.md §3 "Composite primary
// keys") over the PK members. nil unless some consumed member has a set.
func (db *engine) detectTupleIntervalSet(filter *rExpr, rel scopeRel) *pkKeySetPlan {
	if filter == nil {
		return nil
	}
	return db.detectTupleIntervalSetConjuncts(flattenConjuncts(filter), rel, nil)
}

// detectTupleIntervalSetConjuncts is detectTupleIntervalSet over an AND-chain's conjuncts; siblings
// admits a sibling column as a member's source (an index-nested-loop inner).
func (db *engine) detectTupleIntervalSetConjuncts(conjuncts []*rExpr, rel scopeRel, siblings columnRanges) *pkKeySetPlan {
	pk := rel.table.PKIndices()
	if len(pk) < 2 {
		return nil
	}
	plan := &pkKeySetPlan{memberCount: len(pk), allFixed: true}
	for _, ci := range pk {
		if ty, ok := rel.table.Columns[ci].Type.AsScalar(); !ok || !ty.IsFixedWidth() {
			plan.allFixed = false
		}
	}
	members, hasSet := db.walkTupleMembers(conjuncts, rel, pk, siblings)
	if !hasSet {
		return nil
	}
	plan.tuple = members
	return plan
}

// flattenConjuncts lists the top-level AND-chain's conjuncts in order.
func flattenConjuncts(filter *rExpr) []*rExpr {
	if filter == nil {
		return nil
	}
	var conjuncts []*rExpr
	var flatten func(*rExpr)
	flatten = func(e *rExpr) {
		if e.kind == reAnd {
			flatten(e.lhs)
			flatten(e.rhs)
			return
		}
		conjuncts = append(conjuncts, e)
	}
	flatten(filter)
	return conjuncts
}

// walkTupleMembers walks key columns cols in key order as tuple-set members (cost.md §3 "Composite
// primary keys"), each with its own set (the first reducing OR/IN/ANY conjunct) and clip (every
// direct comparison). A point member continues the walk; the first other member with a set or clip
// is the final member and ends it. hasSet reports that some consumed member has a set.
func (db *engine) walkTupleMembers(conjuncts []*rExpr, rel scopeRel, cols []int, siblings columnRanges) (members []tupleMember, hasSet bool) {
	for _, ci := range cols {
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			break
		}
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			break
		}
		colColl := ""
		if coll != nil {
			colColl = coll.Name
		}
		keyIdx := rel.offset + ci
		var specs []intervalSpec
		found := -1
		for i, e := range conjuncts {
			if e.kind != reOr && e.kind != reQuantified && e.kind != reSubquery && e.kind != reInValues {
				continue
			}
			if reduced, ok := reduceIntervalUnion(e, keyIdx, ty, colColl, siblings); ok {
				specs, found = reduced, i
				break
			}
		}
		var clip []boundTerm
		hasEq := false
		for i, e := range conjuncts {
			if i == found {
				continue
			}
			if t, ok := asBoundTerm(e, columnMatch(keyIdx), ty, colColl, siblings); ok {
				clip = append(clip, t)
				hasEq = hasEq || t.op == opEq
			}
		}
		m := tupleMember{name: rel.table.Columns[ci].Name, column: ci, colType: ty, coll: coll, specs: specs, clip: clip}
		if hasEq || (specs != nil && pointSpecs(specs)) {
			m.point = true
			members = append(members, m)
			hasSet = hasSet || specs != nil
			continue
		}
		if specs != nil || len(clip) > 0 {
			members = append(members, m)
			hasSet = hasSet || specs != nil
		}
		break
	}
	return members, hasSet
}

// pointSpecs reports whether every disjunct is a point: an `= ANY` array or a conjunction of
// equalities.
func pointSpecs(specs []intervalSpec) bool {
	for _, spec := range specs {
		if spec.array != nil {
			continue
		}
		if len(spec.terms) == 0 {
			return false
		}
		for _, t := range spec.terms {
			if t.op != opEq {
				return false
			}
		}
	}
	return true
}

func reduceIntervalUnion(e *rExpr, keyIdx int, keyType scalarType, colColl string, siblings columnRanges) ([]intervalSpec, bool) {
	if e == nil {
		return nil, false
	}
	if e.kind == reOr {
		l, lok := reduceIntervalUnion(e.lhs, keyIdx, keyType, colColl, siblings)
		if !lok {
			return nil, false
		}
		r, rok := reduceIntervalUnion(e.rhs, keyIdx, keyType, colColl, siblings)
		if !rok {
			return nil, false
		}
		return append(l, r...), true
	}
	if spec, ok := asArrayPointSpec(e, keyIdx, keyType, colColl, siblings); ok {
		return []intervalSpec{spec}, true
	}
	// A leaf is an AND of same-key comparisons (BETWEEN's resolved shape) and at most one `= ANY`
	// array, whose points those comparisons clip.
	var terms []boundTerm
	var array *rExpr
	var walk func(*rExpr) bool
	walk = func(x *rExpr) bool {
		if x.kind == reAnd {
			return walk(x.lhs) && walk(x.rhs)
		}
		if t, ok := asBoundTerm(x, columnMatch(keyIdx), keyType, colColl, siblings); ok {
			terms = append(terms, t)
			return true
		}
		if array == nil {
			if spec, ok := asArrayPointSpec(x, keyIdx, keyType, colColl, siblings); ok {
				array = spec.array
				return true
			}
		}
		return false
	}
	if walk(e) && (len(terms) > 0 || array != nil) {
		return []intervalSpec{{terms: terms, array: array}}, true
	}
	return nil, false
}

// asArrayPointSpec reports whether e is `key = ANY(array)` whose array elements are points of the
// key (cost.md §3 "OR / IN-list"): ANY (not ALL) equality on the bare key column, the key's own
// collation (as asBoundTerm), and an array that is constant for the whole scan. The element type is
// exactly the key's type, or a promoted type whose values convert exactly to the key (elemPromoted);
// a promoted array must be readable as a value, since its elements convert through pointValue.
func asArrayPointSpec(e *rExpr, keyIdx int, keyType scalarType, colColl string, siblings columnRanges) (intervalSpec, bool) {
	if (e.kind == reSubquery || e.kind == reInValues) && isSubqueryPointSource(e, keyIdx, colColl) {
		return intervalSpec{array: e}, true
	}
	if e.kind != reQuantified || e.op != opEq || e.quantAll || e.rowWise || !(e.elemExact || e.elemPromoted) || !isColumn(e.lhs, keyIdx) {
		return intervalSpec{}, false
	}
	cmpColl := ""
	if e.collation != nil {
		cmpColl = e.collation.Name
	}
	if cmpColl != colColl || !isArrayPointSource(e.rhs, keyType, e.elemExact, siblings) {
		return intervalSpec{}, false
	}
	return intervalSpec{array: e.rhs}, true
}

// specsHaveSubquery reports a membership-subquery point source among an interval set's disjuncts.
func specsHaveSubquery(specs []intervalSpec) bool {
	for _, spec := range specs {
		if spec.array != nil && (spec.array.kind == reSubquery || spec.array.kind == reInValues) {
			return true
		}
	}
	return false
}

// isSubqueryPointSource reports whether e is an uncorrelated membership subquery whose value list is
// a key's point source once folded (cost.md §3 "OR / IN-list"): `key IN (SELECT …)` or `key =
// ANY(SELECT …)` with a single column of the key's type (or a promoted integer/decimal one), on the
// bare key column, or the folded `IN` list a mutation plans over. Its values are read after the fold
// replaces the node in place; an IN compares by byte identity, so a collated key is left alone.
func isSubqueryPointSource(e *rExpr, keyIdx int, colColl string) bool {
	if !(e.elemExact || e.elemPromoted) || e.rowValued || !isColumn(e.lhs, keyIdx) {
		return false
	}
	if e.kind == reInValues {
		return !e.negated && colColl == ""
	}
	if queryPlanReferencesOuter(e.subPlan, 0) {
		return false
	}
	switch e.subKind {
	case sqIn:
		return !e.negated && colColl == ""
	case sqQuantified:
		cmpColl := ""
		if e.collation != nil {
			cmpColl = e.collation.Name
		}
		return e.op == opEq && !e.quantAll && cmpColl == colColl
	}
	return false
}

// isArrayPointSource reports whether an `= ANY` array operand is constant for the whole scan and
// readable without evaluation: a bind parameter, a folded array constant, a NULL, a correlated
// outer column, or — for an exact element type only — a one-dimensional ARRAY[...] whose every
// element is a constant key source.
func isArrayPointSource(e *rExpr, keyType scalarType, exact bool, siblings columnRanges) bool {
	switch e.kind {
	case reParam, reConstArray, reConstNull, reOuterColumn:
		return true
	case reColumn:
		// Index-nested-loop: a sibling array column of the selected physical-left relation.
		return siblings.contains(e.index)
	case reArray:
		if e.nested || !exact {
			return false
		}
		for _, elem := range e.sargs {
			if !isConstSource(elem, keyType, siblings) {
				return false
			}
		}
		return true
	}
	return false
}

// pointPromotable reports whether `key = ANY(elem[])` over a key of type key and elements of a
// different type elem still reads as key points (cost.md §3 "OR / IN-list"): every element value
// either converts exactly to the key's type or equals no key value. That holds for integers of
// different widths and for an integer against a decimal in either direction.
func pointPromotable(key, elem scalarType) bool {
	return (key.IsInteger() || key.IsDecimal()) && (elem.IsInteger() || elem.IsDecimal()) &&
		!(key.IsDecimal() && elem.IsDecimal())
}

// pointValue converts an element value of a `key = ANY(array)` to the key's type (cost.md §3 "OR /
// IN-list"). ok=false means no key value can equal the element — an integer outside the key's width,
// a decimal with a fractional part or outside i64 — so it contributes no point. A NULL, and a value
// already of the key's type, pass through unchanged.
func pointValue(keyType scalarType, v Value) (Value, bool) {
	switch {
	case v.Kind == ValNull:
		return v, true
	case keyType.IsInteger() && v.Kind == ValInt:
		return v, keyType.InRange(v.Int)
	case keyType.IsInteger() && v.Kind == ValDecimal:
		d := *v.decimal()
		n, ok := d.ToInt64Round()
		if !ok || decimalFromInt64(n).CmpValue(d) != 0 || !keyType.InRange(n) {
			return Value{}, false
		}
		return IntValue(n), true
	case keyType.IsDecimal() && v.Kind == ValInt:
		return DecimalValue(decimalFromInt64(v.Int)), true
	}
	return v, true
}

// arrayPointKeys encodes an array spec's elements as key points, in element order: a NULL array,
// a NULL element, and an element no key can equal (an out-of-range integer, a non-integral decimal
// against an integer key, an unmapped collated text) contribute no point, exactly as the equivalent
// IN-list's disjuncts would.
func arrayPointKeys(keyType scalarType, src *rExpr, params []Value, outer []storedRow, coll *Collation, left storedRow) ([][]byte, bool) {
	var keys [][]byte
	switch src.kind {
	case reSubquery:
		// A membership subquery not yet folded has no value list: the caller widens to every key.
		return nil, false
	case reQuantified:
		// A folded `= ANY(SELECT …)`: the constant array the fold produced.
		return arrayPointKeys(keyType, src.rhs, params, outer, coll, left)
	case reInValues:
		return valuePointKeys(keyType, src.list, coll), true
	}
	if src.kind == reArray {
		for _, elem := range src.sargs {
			if key, isNull, ok := encodeBoundKey(keyType, elem, params, outer, coll, left); ok && !isNull {
				keys = append(keys, key)
			}
		}
		return keys, true
	}
	var v Value
	switch src.kind {
	case reParam:
		v = params[src.index]
	case reOuterColumn:
		v = outer[len(outer)-src.level][src.index]
	case reConstArray:
		v = arrayValueOf(src.cArray)
	case reColumn:
		// Index-nested-loop: the sibling column of the current combined left-hand row.
		if src.index < len(left) {
			v = left[src.index]
		}
	}
	if v.Kind != ValArray {
		return nil, true
	}
	return valuePointKeys(keyType, v.arrayVal().Elements, coll), true
}

// valuePointKeys encodes element values as key points, in order: a NULL element and one no key can
// equal contribute none.
func valuePointKeys(keyType scalarType, elems []Value, coll *Collation) [][]byte {
	var keys [][]byte
	for _, elem := range elems {
		point, ok := pointValue(keyType, elem)
		if !ok {
			continue
		}
		if key, isNull, ok := encodeValueKey(keyType, point, coll); ok && !isNull {
			keys = append(keys, key)
		}
	}
	return keys
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
	candidates := inventoryINLCandidates(on, whereFilter, rel, columnRanges{{start: 0, end: rel.offset}}, db)
	if len(candidates) == 0 {
		return nil
	}
	return candidates[0].bound
}

// partialIndexImplied reports whether the WHERE filter implies a partial index's resolved predicate
// (indexes.md §9.1). An index whose predicate does not resolve (an unparsable one) is never implied.
func (db *engine) partialIndexImplied(filter *rExpr, table *catTable, offset int, idx indexDef) bool {
	rindex, err := db.resolveIndex(table, idx)
	if err != nil || rindex.Predicate == nil {
		return false
	}
	return filterImpliesPredicate(filter, rindex.Predicate, offset)
}

// inlPartialAdmissible reports whether a PARTIAL index may serve as an index-nested-loop inner
// (indexes.md §9.3): the join's own on must imply its predicate, or on together with the where must
// imply a NULL-rejecting predicate. A WHERE conjunct holds only for the final rows, and a later outer
// join may NULL-extend this relation; a NULL-rejecting predicate makes every such row fail the
// implied WHERE anyway.
func (db *engine) inlPartialAdmissible(on, whereFilter *rExpr, rel scopeRel, idx indexDef) bool {
	rindex, err := db.resolveIndex(rel.table, idx)
	if err != nil || rindex.Predicate == nil {
		return false
	}
	pred := rindex.Predicate
	if on != nil && filterImpliesPredicate(on, pred, rel.offset) {
		return true
	}
	var filters []*rExpr
	if on != nil {
		filters = append(filters, on)
	}
	if whereFilter != nil {
		filters = append(filters, whereFilter)
	}
	return predicateRejectsNull(pred) && filtersImplyPredicate(filters, pred, rel.offset)
}

// inventoryINLCandidates returns every sibling-dependent access path for rel in canonical
// estimator order. siblingColumns is the selected physical-left relation's logical slot interval;
// this is deliberately independent of FROM ordinal so P7 can cost the reverse orientation.
func inventoryINLCandidates(on *rExpr, whereFilter *rExpr, rel scopeRel, siblingColumns columnRanges, db *engine) []scanCandidate {
	// A host-attached inner relation full-scans per outer row this slice (attached-databases.md §8):
	// the seek would resolve its index store unscoped. Index-nested-loop over an attachment is a
	// perf follow-on.
	if rel.isAttachment() {
		return nil
	}
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
			if t, ok := asBoundTerm(e, columnMatch(keyIdx), ty, colColl, siblingColumns); ok {
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
	candidates := make([]scanCandidate, 0, 1+len(rel.table.Indexes))
	// Primary-key bound first (the row's own key — range-capable, strictly cheaper).
	if bp := db.detectPKBound([]*rExpr{on, whereFilter}, rel, siblingColumns); bp != nil {
		has := false
		for _, ec := range bp.eqCols {
			for _, src := range ec.srcs {
				if src.kind == reColumn {
					has = true
				}
			}
			if hasSibling(ec.ranges) {
				has = true
			}
		}
		if hasSibling(bp.rangeTerms) {
			has = true
		}
		if has {
			candidates = append(candidates, storageOrderCandidate(scanCandidatePK, "", &scanBound{pk: bp}, whereFilter))
		}
	}
	// Every leading secondary-index equality bound to the selected sibling.
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexBtree {
			continue
		}
		// A PARTIAL index holds only its qualifying rows (indexes.md §9), so it is an INL inner only
		// when the join's conditions imply its predicate (§9.3). One whose predicate did not parse
		// never resolves, so it stays out.
		if idx.Predicate != nil && !db.inlPartialAdmissible(on, whereFilter, rel, idx) {
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
		// Every trailing column is skipped by its self-delimiting slot (indexes.md §5.1).
		suffix := make([]keySlot, 0, len(cols)-1)
		unskippableTail := false
		for j, c := range cols[1:] {
			col := rel.table.Columns[c]
			s, ok := col.Type.AsScalar()
			if !ok {
				unskippableTail = true
				break
			}
			slot, ok := keySlotForScalar(s, col.Collation != "")
			if !ok {
				unskippableTail = true
				break
			}
			suffix = append(suffix, slot.withOrder(idx.keyOrder(j+1)))
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
			// §3 "index-nested-loop"). suffix holds the trailing columns' slots (columns[1:]),
			// skipped past the single equality slot.
			bound := &scanBound{index: &indexBoundPlan{
				nameKey: idx.Name,
				eqCols:  []indexEqCol{{colType: ty, coll: coll, srcs: eqs, order: idx.keyOrder(0)}},
				suffix:  suffix,
			}}
			candidates = append(candidates, indexOrderCandidate(scanCandidateBtree, bound.index.nameKey, bound, whereFilter))
		}
	}
	// Interval and tuple sets bound to a sibling (cost.md §3 "JOIN"): a PK or index set one of whose
	// sources is a sibling column — `id = ANY(s.ids)`, `tenant = s.tenant AND id IN (1, 2)` — is
	// evaluated per outer row. A set with constant sources only is the ordinary relation bound.
	conjuncts := append(flattenConjuncts(on), flattenConjuncts(whereFilter)...)
	pkSet := db.detectTupleIntervalSetConjuncts(conjuncts, rel, siblingColumns)
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				if specs, clip := detectIntervalSetConjuncts(conjuncts, rel.offset+pkLocal, sty, coll, siblingColumns); specs != nil {
					pkSet = &pkKeySetPlan{pkType: sty, coll: coll, specs: specs, clip: clip}
				}
			}
		}
	}
	if pkSet != nil && keySetUsesSibling(pkSet.specs, pkSet.clip, pkSet.tuple) {
		candidates = append(candidates, storageOrderCandidate(scanCandidatePKInterval, "", &scanBound{pkSet: pkSet}, whereFilter))
	}
	for _, idx := range rel.table.Indexes {
		if is := db.buildIndexIntervalSetPlanConjuncts(conjuncts, rel, idx, siblingColumns, idx.Predicate == nil || db.inlPartialAdmissible(on, whereFilter, rel, idx)); is != nil && keySetUsesSibling(nil, nil, is.tuple) {
			candidates = append(candidates, indexOrderCandidate(scanCandidateIndexInterval, is.nameKey, &scanBound{indexSet: is}, whereFilter))
		}
	}
	// Opclass sibling bounds follow the cheaper primary-key and ordered-B-tree paths. GiST precedes
	// GIN, matching the ordinary SELECT access-path precedence; each detector admits only a bare
	// column from an earlier sibling, never the indexed relation itself or a later relation.
	filters := []*rExpr{on, whereFilter}
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexGist || len(idx.Keys) != 1 || idx.Predicate != nil {
			continue
		}
		ci := idx.firstColumn()
		colGlobal := rel.offset + ci
		colTy := rel.table.Columns[ci].Type
		for _, filter := range filters {
			if colTy.IsRange() {
				if s, _, ok := gistSiblingMatch(filter, colGlobal, siblingColumns); ok {
					bound := &scanBound{gist: &gistBoundPlan{nameKey: idx.Name, strategy: s, colGlobal: colGlobal}}
					candidates = append(candidates, storageOrderCandidate(scanCandidateGist, bound.gist.nameKey, bound, whereFilter))
					break
				}
			} else if isGistScalarType(colTy) {
				if _, _, ok := gistScalarSiblingMatch(filter, colGlobal, siblingColumns); ok {
					bound := &scanBound{gist: &gistBoundPlan{nameKey: idx.Name, strategy: gistEqual, colGlobal: colGlobal, scalarType: colTy.ScalarTy()}}
					candidates = append(candidates, storageOrderCandidate(scanCandidateGist, bound.gist.nameKey, bound, whereFilter))
					break
				}
			}
		}
	}
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexGin {
			continue
		}
		cols := idx.columnOrdinals()
		// Each key column in key order; the first with a sibling-operand conjunct is the bound.
	columns:
		for pos, ci := range cols {
			colGlobal := rel.offset + ci
			at := rel.table.Columns[ci].Type
			jsonb := at.IsJsonb()
			if at.Array == nil && !jsonb {
				continue
			}
			for _, filter := range filters {
				if s, _, ok := ginSiblingMatch(filter, colGlobal, jsonb, siblingColumns); ok {
					bound := &scanBound{gin: newGinBoundPlan(idx.Name, at, s, colGlobal)}
					bound.gin.tag, bound.gin.tagged = ginColumnTag(len(cols), pos)
					candidates = append(candidates, storageOrderCandidate(scanCandidateGin, bound.gin.nameKey, bound, whereFilter))
					break columns
				}
			}
		}
	}
	sort.SliceStable(candidates, func(i, j int) bool {
		a, b := candidates[i].identity, candidates[j].identity
		if a.kind != b.kind {
			return a.kind < b.kind
		}
		return bytes.Compare([]byte(a.indexName), []byte(b.indexName)) < 0
	})
	return candidates
}

// keySetUsesSibling reports an interval or tuple set with a sibling column among its sources (a
// comparison source, an array operand, or an ARRAY[...] element): an index-nested-loop set.
func keySetUsesSibling(specs []intervalSpec, clip []boundTerm, tuple []tupleMember) bool {
	termsUse := func(terms []boundTerm) bool {
		for _, t := range terms {
			if t.src.kind == reColumn {
				return true
			}
		}
		return false
	}
	specsUse := func(specs []intervalSpec) bool {
		for _, spec := range specs {
			if termsUse(spec.terms) {
				return true
			}
			if a := spec.array; a != nil {
				if a.kind == reColumn {
					return true
				}
				if a.kind == reArray {
					for _, elem := range a.sargs {
						if elem.kind == reColumn {
							return true
						}
					}
				}
			}
		}
		return false
	}
	if specsUse(specs) || termsUse(clip) {
		return true
	}
	for _, m := range tuple {
		if specsUse(m.specs) || termsUse(m.clip) {
			return true
		}
	}
	return false
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

// buildGinBoundForIndex inventories one GIN index when its array column has an accelerable conjunct
// (`col @> const`, `col && const`, `const = ANY(col)`, or `col = const`). The complete inventory
// calls it once per catalog index; legacy selection later chooses the lowest name.
func buildGinBoundForIndex(filter *rExpr, idx indexDef, columns []catColumn, offset int) *ginBoundPlan {
	if filter == nil || idx.Kind != indexGin {
		return nil
	}
	cols := idx.columnOrdinals()
	// A multi-column index serves a query on any of its columns (gin.md §4.4): the first key column,
	// in key order, with a single accelerable conjunct is the bound; failing that, the first with a
	// disjunctive one (§6.3).
	for pos, ci := range cols {
		at := columns[ci].Type
		jsonb := at.IsJsonb()
		if at.Array == nil && !jsonb {
			continue // a GIN column is always an array or jsonb (the CREATE INDEX gate); defensive
		}
		if s, _, ok := ginMatch(filter, offset+ci, jsonb); ok {
			gb := newGinBoundPlan(idx.Name, at, s, offset+ci)
			gb.tag, gb.tagged = ginColumnTag(len(cols), pos)
			return gb
		}
	}
	// No single accelerable conjunct: an OR conjunct whose every arm is one (an IN list over the
	// column) bounds the scan by the union of the arms' gathers (gin.md §6.3).
	for pos, ci := range cols {
		at := columns[ci].Type
		jsonb := at.IsJsonb()
		if at.Array == nil && !jsonb {
			continue
		}
		colGlobal := offset + ci
		or := ginOrMatch(filter, colGlobal, jsonb)
		if or == nil {
			continue
		}
		var arms []ginStrategy
		for _, arm := range ginOrArms(or) {
			s, _, _ := ginMatch(arm, colGlobal, jsonb)
			arms = append(arms, s)
		}
		gb := newGinBoundPlan(idx.Name, at, arms[0], colGlobal)
		gb.orArms = arms
		gb.tag, gb.tagged = ginColumnTag(len(cols), pos)
		return gb
	}
	return nil
}

// ginOrMatch finds the disjunctive GIN conjunct (gin.md §6.3): the first conjunct of the WHERE
// AND-chain that is an OR (an IN list over the column desugars to one) every one of whose disjuncts
// — flattened left to right — holds a constant accelerable conjunct on the indexed column
// (ginMatch). Returns the OR node; ginOrArms lists its disjuncts.
func ginOrMatch(filter *rExpr, colGlobal int, jsonb bool) *rExpr {
	if filter == nil {
		return nil
	}
	switch filter.kind {
	case reAnd:
		if or := ginOrMatch(filter.lhs, colGlobal, jsonb); or != nil {
			return or
		}
		return ginOrMatch(filter.rhs, colGlobal, jsonb)
	case reOr:
		for _, arm := range ginOrArms(filter) {
			if _, _, ok := ginMatch(arm, colGlobal, jsonb); !ok {
				return nil
			}
		}
		return filter
	}
	return nil
}

// ginOrArms is the disjuncts of an OR node, flattened in source order.
func ginOrArms(or *rExpr) []*rExpr {
	if or.kind == reOr {
		return append(ginOrArms(or.lhs), ginOrArms(or.rhs)...)
	}
	return []*rExpr{or}
}

// ginBoundQuery recovers the query node a constant GIN bound gathers from in the plan's filter at
// execution: the single conjunct's query operand, or a disjunctive bound's OR node (gin.md §6.3).
// Plan time and execution use the same matchers, so they agree on the conjunct by construction.
func ginBoundQuery(filter *rExpr, gb *ginBoundPlan) *rExpr {
	if len(gb.orArms) == 0 {
		if _, q, ok := ginMatch(filter, gb.colGlobal, gb.jsonb); ok {
			return q
		}
		return nil
	}
	return ginOrMatch(filter, gb.colGlobal, gb.jsonb)
}

// newGinBoundPlan builds the plan for one GIN index over a column of type colTy (an array — array_ops
// — or jsonb — jsonb_ops).
func newGinBoundPlan(name string, colTy dataType, s ginStrategy, colGlobal int) *ginBoundPlan {
	gb := &ginBoundPlan{nameKey: name, strategy: s, colGlobal: colGlobal, jsonb: colTy.IsJsonb()}
	if colTy.Array != nil {
		gb.elemType = colTy.Array.ScalarTy()
	}
	return gb
}

// ginMatch finds the first WHERE AND-chain conjunct a GIN index on colGlobal accelerates
// (spec/design/gin.md §6): `col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)`
// (membership), or `col = Q` (exact array equality) where the query operand is a constant
// (references no column / outer / subquery). @> is asymmetric (the indexed column must be the LEFT
// operand — `Q @> col` is the non-accelerated <@); && and array = are symmetric; = ANY requires the
// column be ANY's array operand and c the scalar. Returns the strategy and the constant query
// operand (the scalar c for ginMember, the array Q otherwise). Used at plan time (strategy) and exec
// time (recover the operand from plan.filter), so the two agree on the same conjunct by construction.
//
// jsonb selects the opclass: on a jsonb column only the jsonb_ops shapes match (gin.md §2.1 —
// `col @> Q` / `Q <@ col`, `col ? k`, `col ?| K`, `col ?& K`), on an array column only the array_ops
// shapes (so `jsonbcol = Q` never becomes an array-equality bound).
func ginMatch(filter *rExpr, colGlobal int, jsonb bool) (ginStrategy, *rExpr, bool) {
	return ginMatchOperand(filter, colGlobal, jsonb, rexprIsConstant)
}

// ginSiblingMatch is the join counterpart of ginMatch: Q must be a bare column from an earlier
// sibling relation. Expressions, the indexed inner column, and later-sibling columns are rejected.
func ginSiblingMatch(filter *rExpr, colGlobal int, jsonb bool, siblingColumns columnRanges) (ginStrategy, *rExpr, bool) {
	return ginMatchOperand(filter, colGlobal, jsonb, func(e *rExpr) bool {
		return e != nil && e.kind == reColumn && siblingColumns.contains(e.index)
	})
}

func ginMatchOperand(filter *rExpr, colGlobal int, jsonb bool, queryOK func(*rExpr) bool) (ginStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := ginMatchOperand(filter.lhs, colGlobal, jsonb, queryOK); ok {
			return s, q, true
		}
		return ginMatchOperand(filter.rhs, colGlobal, jsonb, queryOK)
	}
	if jsonb {
		// jsonb_ops (gin.md §2.1): the indexed column is the containing operand of @> (`Q <@ col`
		// resolves to the same reJsonContains node) and the base of ? / ?| / ?&.
		switch {
		case filter.kind == reJsonContains && isColumn(filter.lhs, colGlobal) && queryOK(filter.rhs):
			return ginJsonbContains, filter.rhs, true
		case filter.kind == reJsonHasKey && isColumn(filter.lhs, colGlobal) && queryOK(filter.rhs):
			switch filter.hasKey {
			case hkOne:
				return ginJsonbExists, filter.rhs, true
			case hkAny:
				return ginJsonbExistsAny, filter.rhs, true
			default:
				return ginJsonbExistsAll, filter.rhs, true
			}
		// `col @? P` / `col @@ P` (gin.md §2.2): only the operators — the jsonpath functions take vars
		// and a silent flag, and PostgreSQL indexes neither.
		case filter.kind == reJsonPathFn && !filter.jpTz && len(filter.sargs) == 2 &&
			(filter.jpFnKind == jpfExistsSilent || filter.jpFnKind == jpfMatchSilent) &&
			isColumn(filter.sargs[0], colGlobal) && queryOK(filter.sargs[1]):
			if filter.jpFnKind == jpfExistsSilent {
				return ginJsonPathExists, filter.sargs[1], true
			}
			return ginJsonPathMatch, filter.sargs[1], true
		}
		return 0, nil, false
	}
	if filter.kind == reArrayFunc && len(filter.sargs) == 2 {
		a, b := filter.sargs[0], filter.sargs[1]
		switch filter.afunc {
		case afContains:
			if isColumn(a, colGlobal) && queryOK(b) {
				return ginContains, b, true
			}
		case afOverlaps:
			if isColumn(a, colGlobal) && queryOK(b) {
				return ginOverlaps, b, true
			}
			if isColumn(b, colGlobal) && queryOK(a) {
				return ginOverlaps, a, true
			}
		}
	}
	// `col = Q` — exact array equality (gin.md §6). Commutative: the column may be either operand,
	// the constant array Q the other. Recovered operand is Q; ginBoundRows reads it via ginEqual
	// (the @>-superset gather + the residual =). <> is NOT matched (only OpEq). When the column is an
	// array, the other constant operand is necessarily an array too (resolve rejects array/scalar =).
	if filter.kind == reCompare && filter.op == opEq {
		if isColumn(filter.lhs, colGlobal) && queryOK(filter.rhs) {
			return ginEqual, filter.rhs, true
		}
		if isColumn(filter.rhs, colGlobal) && queryOK(filter.lhs) {
			return ginEqual, filter.lhs, true
		}
	}
	// `c = ANY(col)` — the array spelling of membership (gin.md §6): the GIN column must be ANY's
	// ARRAY operand (rhs) and c (the scalar lhs) a constant. Only = ANY (not = ALL, not any other
	// comparison/quantifier — those are not a single-term posting gather). The recovered query
	// operand is the scalar c; ginBoundRows reads it via ginMember.
	if filter.kind == reQuantified && filter.op == opEq && !filter.quantAll &&
		isColumn(filter.rhs, colGlobal) && queryOK(filter.lhs) {
		return ginMember, filter.lhs, true
	}
	return 0, nil, false
}

// buildGistBoundForIndex inventories one single-column GiST index. Multi-column GiST indexes are
// EXCLUDE backing structures and remain constraint-only, never planner candidates; so is the
// partial GiST index a partial EXCLUDE … WHERE builds (gist.md §7.1 — it holds only the
// predicate's rows).
func buildGistBoundForIndex(filter *rExpr, idx indexDef, columns []catColumn, offset int) *gistBoundPlan {
	if filter == nil || idx.Kind != indexGist || len(idx.Keys) != 1 || idx.Predicate != nil {
		return nil
	}
	ci := idx.firstColumn()
	colGlobal := offset + ci
	colTy := columns[ci].Type
	if colTy.IsRange() {
		// range_ops (GX1): a `col && Q` / `col @> Q` conjunct.
		if s, _, ok := gistMatch(filter, colGlobal); ok {
			return &gistBoundPlan{nameKey: idx.Name, strategy: s, colGlobal: colGlobal}
		}
	} else if isGistScalarType(colTy) {
		// scalar `=` opclass (GX2): a `col = Q` conjunct over a fixed-width keyable scalar.
		if _, _, ok := gistScalarMatch(filter, colGlobal); ok {
			return &gistBoundPlan{nameKey: idx.Name, strategy: gistEqual, colGlobal: colGlobal, scalarType: colTy.ScalarTy()}
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
	return gistScalarMatchOperand(filter, colGlobal, rexprIsConstant)
}

func gistScalarSiblingMatch(filter *rExpr, colGlobal int, siblingColumns columnRanges) (gistStrategy, *rExpr, bool) {
	return gistScalarMatchOperand(filter, colGlobal, func(e *rExpr) bool {
		return e != nil && e.kind == reColumn && siblingColumns.contains(e.index)
	})
}

func gistScalarMatchOperand(filter *rExpr, colGlobal int, queryOK func(*rExpr) bool) (gistStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := gistScalarMatchOperand(filter.lhs, colGlobal, queryOK); ok {
			return s, q, true
		}
		return gistScalarMatchOperand(filter.rhs, colGlobal, queryOK)
	}
	if filter.kind == reCompare && filter.op == opEq {
		if isColumn(filter.lhs, colGlobal) && queryOK(filter.rhs) {
			return gistEqual, filter.rhs, true
		}
		if isColumn(filter.rhs, colGlobal) && queryOK(filter.lhs) {
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
	return gistMatchOperand(filter, colGlobal, rexprIsConstant)
}

func gistSiblingMatch(filter *rExpr, colGlobal int, siblingColumns columnRanges) (gistStrategy, *rExpr, bool) {
	return gistMatchOperand(filter, colGlobal, func(e *rExpr) bool {
		return e != nil && e.kind == reColumn && siblingColumns.contains(e.index)
	})
}

func gistMatchOperand(filter *rExpr, colGlobal int, queryOK func(*rExpr) bool) (gistStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := gistMatchOperand(filter.lhs, colGlobal, queryOK); ok {
			return s, q, true
		}
		return gistMatchOperand(filter.rhs, colGlobal, queryOK)
	}
	if filter.kind == reRangeOp && len(filter.sargs) == 2 {
		a, b := filter.sargs[0], filter.sargs[1]
		switch filter.rop {
		case roOverlaps: // && — symmetric in its operands
			if isColumn(a, colGlobal) && queryOK(b) {
				return gistOverlaps, b, true
			}
			if isColumn(b, colGlobal) && queryOK(a) {
				return gistOverlaps, a, true
			}
		case roContains: // @> — the indexed column must be the container (LEFT)
			if isColumn(a, colGlobal) && queryOK(b) {
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
	case reHostFunc:
		// A host function is conservatively NEVER constant (extensibility.md §4.2): it may be
		// volatile, and this slice folds none, so it must not be treated as a literal for estimator
		// selectivity nor as a one-time constant operand for a GiST/GIN index match.
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
	return db.indexScanBoundEntries(tableName, ib.nameKey, ib.suffix, b, prefixByteLen, mask)
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
		// The column's §2.16 present slot (a DESC key inverts its body).
		p = ec.order.pushPresent(p, agreed)
	}
	if ib.rangeTerms == nil {
		b = keyBound{lo: p, loInc: true, hi: prefixSuccessor(p), hiInc: false}
		return b, len(p), boundEmpty(b)
	}
	// Equality prefix P + a range on the next column: the present region of that column's slot
	// [P‖tag, P‖tag+1) (the order's NULL byte sits outside it — a range is never true for NULL),
	// tightened by each term's logical interval mapped through the column's order (§2.16; a DESC
	// column swaps the byte endpoints).
	logical := unboundedBound()
	for _, t := range ib.rangeTerms {
		// A collated range column's probe is the collation's sort key (the stored key form).
		key, isNull, ok := encodeBoundKey(ib.rangeType, t.src, params, outer, ib.rangeColl, left)
		if isNull {
			return keyBound{}, 0, true
		}
		if !ok {
			continue // out-of-range endpoint: drop this half-bound (a wider, still-sound scan)
		}
		switch t.op {
		case opGe:
			logical = intersectLo(logical, key, true)
		case opGt:
			logical = intersectLo(logical, key, false)
		case opLt:
			logical = intersectHi(logical, key, false)
		case opLe:
			logical = intersectHi(logical, key, true)
		case opEq: // defensive — an equality never reaches rangeTerms, but treat it as [v, v]
			logical = intersectLo(logical, key, true)
			logical = intersectHi(logical, key, true)
		}
	}
	if boundEmpty(logical) {
		return keyBound{}, 0, true
	}
	b = prefixedInterval(p, indexLogicalInterval(logical, ib.rangeOrder))
	return b, len(p), boundEmpty(b)
}

// indexScanBound range-scans the index B-tree over an already-built key bound and point-looks-up
// each admitted entry's row, returning them in index-entry order with the scan's up-front (pages,
// slabs) block — the index-tree nodes overlapping the range plus, per entry, the row's point
// lookup. prefixByteLen is the equality-prefix byte length skipped before the fixed-width
// suffix-skip that recovers each entry's row storage key (indexes.md §5.1). Shared by the
// access-predicate bound (indexBoundRows) and the OR / IN-list key-set probes so both
// drive the identical per-entry fetch — same cost by construction.
func (db *engine) indexScanBound(tableName, nameKey string, suffix []keySlot, b keyBound, prefixByteLen int, mask []bool) (rows []storedRow, pages, slabs int, err error) {
	entries, pages, slabs, err := db.indexScanBoundEntries(tableName, nameKey, suffix, b, prefixByteLen, mask)
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
func (db *engine) indexScanBoundEntries(tableName, nameKey string, suffix []keySlot, b keyBound, prefixByteLen int, mask []bool) (out []entry, pages, slabs int, err error) {
	return indexStoreBoundEntries(db.lkpIndexStore(nameKey), db.lkpStore(tableName), suffix, b, prefixByteLen, mask)
}

// indexStoreBoundEntries is indexScanBoundEntries over already-resolved index and table stores, for
// a caller that must not take the temp-first name walk (an FK action scoped to its own domain,
// constraints.md §6.6).
func indexStoreBoundEntries(istore, store *tableStore, suffix []keySlot, b keyBound, prefixByteLen int, mask []bool) (out []entry, pages, slabs int, err error) {
	// The index store has no payload columns, so its mask is empty and its fused scan contributes
	// only the index-tree page_read count (no spill/compress units).
	entries, pages, _, err := istore.RangeScanWithUnits(b, nil)
	if err != nil {
		return nil, 0, 0, err
	}
	for _, e := range entries {
		// Skip the equality prefix by its known byte length, then each remaining key component by
		// its self-delimiting slot (indexes.md §5.1); the suffix after them is the row's storage
		// key (indexes.md §3). A corrupt entry yields an empty storage key (no row).
		rowKey := indexEntryRowKey(e.Key, prefixByteLen, suffix)
		row, ok, n, sl, err := store.GetWithUnits(rowKey, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		pages += n
		slabs += sl
		if !ok {
			return nil, 0, 0, newError(DataCorrupted, "an index entry references a stored row")
		}
		out = append(out, entry{Key: append([]byte(nil), rowKey...), Row: row})
	}
	return out, pages, slabs, nil
}

func buildLogicalInterval(keyType scalarType, terms []boundTerm, params []Value, outer []storedRow, coll *Collation, left storedRow) (keyBound, bool) {
	b := unboundedBound()
	for _, t := range terms {
		key, isNull, ok := encodeBoundKey(keyType, t.src, params, outer, coll, left)
		if isNull {
			return keyBound{}, true
		}
		if !ok {
			if t.op == opEq {
				return keyBound{}, true
			}
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
	return b, boundEmpty(b)
}

func intersectBounds(a, b keyBound) keyBound {
	out := a
	if b.lo != nil {
		out = intersectLo(out, b.lo, b.loInc)
	}
	if b.hi != nil {
		out = intersectHi(out, b.hi, b.hiInc)
	}
	return out
}

func canonicalIntervalSet(keyType scalarType, specs []intervalSpec, clipTerms []boundTerm, params []Value, outer []storedRow, coll *Collation, left storedRow) []keyBound {
	clip := unboundedBound()
	if len(clipTerms) > 0 {
		var empty bool
		clip, empty = buildLogicalInterval(keyType, clipTerms, params, outer, coll, left)
		if empty {
			return nil
		}
	}
	intervals := make([]keyBound, 0, len(specs))
	for _, spec := range specs {
		if spec.array != nil {
			// An array disjunct's points are clipped by its own leaf's comparisons, then the clip.
			pointClip := clip
			if len(spec.terms) > 0 {
				b, empty := buildLogicalInterval(keyType, spec.terms, params, outer, coll, left)
				if empty {
					continue
				}
				pointClip = intersectBounds(b, clip)
			}
			keys, known := arrayPointKeys(keyType, spec.array, params, outer, coll, left)
			if !known {
				if !boundEmpty(pointClip) {
					intervals = append(intervals, pointClip)
				}
				continue
			}
			for _, key := range keys {
				b := intersectBounds(intersectHi(intersectLo(unboundedBound(), key, true), key, true), pointClip)
				if !boundEmpty(b) {
					intervals = append(intervals, b)
				}
			}
			continue
		}
		b, empty := buildLogicalInterval(keyType, spec.terms, params, outer, coll, left)
		if empty {
			continue
		}
		b = intersectBounds(b, clip)
		if !boundEmpty(b) {
			intervals = append(intervals, b)
		}
	}
	return mergeKeyIntervals(intervals, keyType.IsFixedWidth())
}

// mergeKeyIntervals sorts intervals by lower endpoint and merges those that overlap or touch, and —
// when every key byte is fixed-width — closed intervals adjacent under prefixSuccessor.
func mergeKeyIntervals(intervals []keyBound, fixedWidth bool) []keyBound {
	sort.SliceStable(intervals, func(i, j int) bool {
		if intervals[i].lo == nil {
			return intervals[j].lo != nil
		}
		if intervals[j].lo == nil {
			return false
		}
		if c := bytes.Compare(intervals[i].lo, intervals[j].lo); c != 0 {
			return c < 0
		}
		return intervals[i].loInc && !intervals[j].loInc
	})
	out := intervals[:0]
	for _, next := range intervals {
		if len(out) == 0 {
			out = append(out, next)
			continue
		}
		cur := &out[len(out)-1]
		merge := cur.hi == nil || next.lo == nil
		if !merge {
			cmp := bytes.Compare(next.lo, cur.hi)
			merge = cmp < 0 || (cmp == 0 && (cur.hiInc || next.loInc))
			if !merge && fixedWidth && cur.hiInc && next.loInc {
				merge = bytes.Equal(prefixSuccessor(cur.hi), next.lo)
			}
		}
		if !merge {
			out = append(out, next)
			continue
		}
		if cur.hi == nil {
			continue
		}
		if next.hi == nil {
			cur.hi, cur.hiInc = nil, false
		} else if cmp := bytes.Compare(next.hi, cur.hi); cmp > 0 || (cmp == 0 && next.hiInc) {
			cur.hi, cur.hiInc = next.hi, next.hiInc
		}
	}
	return out
}

// memberPoints is a point member's sorted, de-duplicated encoded points (never merged), each inside
// the member's clip. A member with no set has the single point its equalities agree on.
func memberPoints(m tupleMember, params []Value, outer []storedRow, left storedRow) ([][]byte, bool) {
	clip, empty := buildLogicalInterval(m.colType, m.clip, params, outer, m.coll, left)
	if empty {
		return nil, true
	}
	if m.specs == nil {
		return [][]byte{clip.lo}, true
	}
	var keys [][]byte
	keep := func(key []byte, within keyBound) {
		if !boundEmpty(intersectBounds(keyBound{lo: key, loInc: true, hi: key, hiInc: true}, within)) {
			keys = append(keys, key)
		}
	}
	for _, spec := range m.specs {
		if spec.array != nil {
			// The array's points are clipped by their own leaf's comparisons as well as the clip.
			within := clip
			if len(spec.terms) > 0 {
				b, empty := buildLogicalInterval(m.colType, spec.terms, params, outer, m.coll, left)
				if empty {
					continue
				}
				within = intersectBounds(b, clip)
			}
			keys, known := arrayPointKeys(m.colType, spec.array, params, outer, m.coll, left)
			if !known {
				return nil, false
			}
			for _, key := range keys {
				keep(key, within)
			}
			continue
		}
		if b, empty := buildLogicalInterval(m.colType, spec.terms, params, outer, m.coll, left); !empty {
			keep(b.lo, clip)
		}
	}
	sort.SliceStable(keys, func(i, j int) bool { return bytes.Compare(keys[i], keys[j]) < 0 })
	out := keys[:0]
	for _, key := range keys {
		if len(out) == 0 || !bytes.Equal(out[len(out)-1], key) {
			out = append(out, key)
		}
	}
	return out, true
}

// finalMemberIntervals is the final member's canonical logical intervals: its set clipped by its
// clip, or with no set the one interval its clip describes.
func finalMemberIntervals(m tupleMember, params []Value, outer []storedRow, left storedRow) []keyBound {
	if m.specs != nil {
		return canonicalIntervalSet(m.colType, m.specs, m.clip, params, outer, m.coll, left)
	}
	b, empty := buildLogicalInterval(m.colType, m.clip, params, outer, m.coll, left)
	if empty {
		return nil
	}
	return []keyBound{b}
}

func concatKey(p, k []byte) []byte {
	out := make([]byte, 0, len(p)+len(k))
	return append(append(out, p...), k...)
}

// expandTuplePrefixes evaluates a tuple set's members in key order (cost.md §3): the cartesian
// product of the point members' keys — bare for a PK (orders == nil), each member's §2.16 present
// slot under its orders[i] for an index — as prefixes in byte order, and the final member's
// canonical logical intervals when it is consumed. A later member with more than one point that
// would take the running product over maxKeySetProbes ends the walk unconsumed. An empty member
// empties the set: no prefixes.
func expandTuplePrefixes(tuple []tupleMember, orders []keyOrder, params []Value, outer []storedRow, left storedRow) (prefixes [][]byte, final []keyBound, consumed int, hasFinal bool) {
	prefixes = [][]byte{nil}
	for i, m := range tuple {
		var count int
		var points [][]byte
		if m.point {
			var known bool
			if points, known = memberPoints(m, params, outer, left); !known {
				// A membership subquery not yet folded has no points: the set widens to the prefixes
				// so far, as at the probe cap.
				final = nil
				break
			}
			count = len(points)
		} else {
			final = finalMemberIntervals(m, params, outer, left)
			count = len(final)
		}
		if count == 0 {
			return nil, nil, 0, false
		}
		if i > 0 && count > 1 && count > maxKeySetProbes/len(prefixes) {
			final = nil
			break
		}
		consumed++
		if !m.point {
			hasFinal = true
			break
		}
		// An index member's points become its ordered slots, re-sorted into physical byte order (a
		// DESC member's inverted bodies reverse the logical order).
		if orders != nil {
			slots := make([][]byte, len(points))
			for j, k := range points {
				slots[j] = orders[i].presentSlot(k)
			}
			sort.Slice(slots, func(a, b int) bool { return bytes.Compare(slots[a], slots[b]) < 0 })
			points = slots
		}
		next := make([][]byte, 0, len(prefixes)*count)
		for _, p := range prefixes {
			for _, k := range points {
				next = append(next, concatKey(p, k))
			}
		}
		prefixes = next
	}
	return prefixes, final, consumed, hasFinal
}

// tupleIntervalSet resolves a composite-PK tuple set (cost.md §3): each prefix a tuple point, a
// prefix range, or the final member's intervals under it, then canonicalized.
func tupleIntervalSet(ks *pkKeySetPlan, params []Value, outer []storedRow, left storedRow) []keyBound {
	prefixes, final, consumed, hasFinal := expandTuplePrefixes(ks.tuple, nil, params, outer, left)
	complete := consumed == ks.memberCount && !hasFinal
	out := make([]keyBound, 0, len(prefixes)*max(1, len(final)))
	for _, p := range prefixes {
		switch {
		case complete:
			out = append(out, keyBound{lo: p, loInc: true, hi: append([]byte(nil), p...), hiInc: true})
		case !hasFinal:
			b := keyBound{lo: p, loInc: true, hi: prefixSuccessor(p)}
			if len(p) == 0 {
				b.lo = nil
			}
			out = append(out, b)
		default:
			for _, iv := range final {
				var b keyBound
				switch {
				case iv.lo == nil:
					if len(p) > 0 {
						b.lo, b.loInc = p, true
					}
				case iv.loInc:
					b.lo, b.loInc = concatKey(p, iv.lo), true
				default:
					next := prefixSuccessor(concatKey(p, iv.lo))
					if next == nil {
						continue
					}
					b.lo, b.loInc = next, true
				}
				switch {
				case iv.hi == nil:
					b.hi = prefixSuccessor(p)
				case iv.hiInc:
					b.hi = prefixSuccessor(concatKey(p, iv.hi))
				default:
					b.hi = concatKey(p, iv.hi)
				}
				if !boundEmpty(b) {
					out = append(out, b)
				}
			}
		}
	}
	return mergeKeyIntervals(out, ks.allFixed)
}

// pkKeySetRows executes canonical logical intervals over the row's own B-tree. It retains storage
// keys for mutation consumers and sums each disjoint interval's page/slab block.
func (db *engine) pkKeySetRows(store *tableStore, ks *pkKeySetPlan, params []Value, outer []storedRow, mask []bool, left storedRow, masked bool) (entries []entry, pages, slabs int, err error) {
	for _, b := range ks.intervals(params, outer, left) {
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

// indexKeySetRows probes the set's canonical intervals in the secondary index's key space. Each admitted index entry point-looks-up the table row; the complete WHERE remains residual.
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
	for _, pr := range ks.probes(params, outer, left) {
		r, p, sl, err := db.indexScanBoundEntries(tableName, ks.nameKey, pr.suffix, pr.bound, pr.prefixLen, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		entries = append(entries, r...)
		pages += p
		slabs += sl
	}
	return entries, pages, slabs, nil
}

// indexLogicalInterval maps one key element's LOGICAL interval (bare order-preserving key bytes,
// ascending) to its physical slot interval under order (encoding.md §2.16): the present region
// [tag, tag+1) of the slot, tightened by the logical bounds. An ascending order keeps the endpoints
// (v ↦ tag‖v); a DESC order inverts each body, which reverses memcmp order over the prefix-free key
// bodies, so the logical lower bound becomes the physical upper bound and vice versa — `≥ v` ↦
// `< succ(tag‖~v)`, `> v` ↦ `< tag‖~v`, `< v` ↦ `≥ succ(tag‖~v)`, `≤ v` ↦ `≥ tag‖~v`. The result is
// half-open (lo inclusive, hi exclusive) whenever both ends are bounded; a nil hi (the DESC NULLS
// FIRST present region tops the slot at 0xFF) is unbounded.
func indexLogicalInterval(logical keyBound, order keyOrder) keyBound {
	tag := order.presentTag()
	b := keyBound{lo: []byte{tag}, loInc: true, hi: prefixSuccessor([]byte{tag}), hiInc: false}
	tighten := func(key []byte, logicalLo, inc bool) {
		slot := order.presentSlot(key)
		// An ascending order keeps the side; DESC flips it.
		physLo := logicalLo != order.Desc
		switch {
		case physLo && inc:
			b = intersectLo(b, slot, true)
		case physLo && !inc:
			if next := prefixSuccessor(slot); next != nil {
				b = intersectLo(b, next, true)
			} else {
				b.lo = slot
				b.hi = append([]byte(nil), slot...)
				b.hiInc = false
			}
		case !physLo && inc:
			if next := prefixSuccessor(slot); next != nil {
				b = intersectHi(b, next, false)
			}
		default:
			b = intersectHi(b, slot, false)
		}
	}
	if logical.lo != nil {
		tighten(logical.lo, true, logical.loInc)
	}
	if logical.hi != nil {
		tighten(logical.hi, false, logical.hiInc)
	}
	return b
}

// prefixedInterval places a one-slot physical interval (indexLogicalInterval) under the encoded
// prefix p; an unbounded upper end (the DESC NULLS FIRST present region tops the slot) becomes
// succ(p).
func prefixedInterval(p []byte, phys keyBound) keyBound {
	var b keyBound
	if phys.lo != nil {
		b.lo, b.loInc = concatKey(p, phys.lo), phys.loInc
	} else {
		b.lo, b.loInc = append([]byte(nil), p...), true
	}
	if phys.hi != nil {
		b.hi, b.hiInc = concatKey(p, phys.hi), phys.hiInc
	} else {
		b.hi, b.hiInc = prefixSuccessor(p), false
	}
	return b
}

// executeMutationScan executes a planned UPDATE/DELETE access path into the normalized keyed-row
// batch. It owns the access-method switch that used to be duplicated inline in both DML executors;
// per-row guards, storage_row_read, residual evaluation, and the phase-2 writes stay with the caller.
func (db *engine) executeMutationScan(plan mutationScanPlan, tableName string, params []Value, env *evalEnv, meter *costMeter, mask []bool) (mutationScanBatch, error) {
	// A WHERE contradiction (planner.md §3.1) reads no page and no row, and so charges nothing.
	if plan.contradiction {
		return mutationScanBatch{empty: true}, nil
	}
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
		query := ginBoundQuery(plan.filter, b.gin)
		entries, pages, slabs, err := db.ginBoundRows(tableName, b.gin, query, nil, env, meter, mask, false)
		return mutationScanBatch{entries: entries, pages: pages, slabs: slabs}, err
	}
	if b.gist != nil {
		var query *rExpr
		if q, ok := gistQueryOperand(plan.filter, b.gist); ok {
			query = q
		}
		entries, pages, slabs, err := db.gistBoundRows(tableName, b.gist, query, nil, env, meter, mask, false)
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
// query operand, extracts its terms + mode via the array_ops opclass (an array for @>/&&/=;
// a single scalar term for = ANY — ginMember; the array's distinct non-NULL terms for = — ginEqual),
// gathers each term's posting list (a prefix range scan of the GIN entry tree), combines them by mode
// (@>, = ANY, and = → intersection, && → union) into the candidate storage-key set, and
// point-looks-up each candidate in storage-key order. The original predicate stays the residual WHERE
// filter (re-applied downstream), so the result is always correct. Returns the candidate rows + the
// scan's up-front (pages, slabs); gin_entry (per posting entry visited) is charged on meter directly.
// Degenerate queries (gin.md §6): a NULL Q, an @> whose Q holds a NULL element, an && with
// no non-NULL term, and a NULL = ANY scalar are provably empty; @> '{}' and array = with no non-NULL
// term fall back to the full scan.
// ginBoundRows gathers a GIN-bounded scan's candidate rows as (storage key, row) Entry pairs
// (the candidate set IS the storage keys), with the up-front (page_read nodes, value_decompress
// slabs) block. SELECT drops the keys; UPDATE/DELETE keep them to rewrite/remove the rows
// (gin.md §6). GinEntry is charged inside (during the gather); the caller charges the block.
func (db *engine) ginBoundRows(tableName string, gb *ginBoundPlan, query *rExpr, queryRow storedRow, env *evalEnv, meter *costMeter, mask []bool, keysOnly bool) (out []entry, pages, slabs int, err error) {
	store := db.lkpStore(tableName)
	if query == nil {
		return nil, 0, 0, nil
	}
	// Extract the query's terms (extract_query_terms) — a pure planning step, NOT metered (cost.md
	// §3): evaluate Q on a scratch meter. queryRow is nil for an ordinary constant bound and the
	// combined left-hand row for an index-nested-loop sibling bound. A disjunctive bound's query is its
	// OR node (gin.md §6.3): every arm's operand is extracted, in source order, before any posting
	// list is read.
	type ginArm struct {
		strategy ginStrategy
		operand  *rExpr
	}
	arms := []ginArm{{gb.strategy, query}}
	if len(gb.orArms) > 0 {
		arms = arms[:0]
		for _, arm := range ginOrArms(query) {
			s, q, _ := ginMatch(arm, gb.colGlobal, gb.jsonb)
			arms = append(arms, ginArm{s, q})
		}
	}
	var queries []*ginTree
	for _, arm := range arms {
		qv, err := arm.operand.eval(queryRow, env, env.exec.session.scratchMeter())
		if err != nil {
			return nil, 0, 0, err
		}
		if gb.jsonb {
			if qv, err = residentValue(qv); err != nil {
				return nil, 0, 0, err
			}
		}
		q := ginQueryTerms(gb.opclass(), arm.strategy, qv)
		switch q.kind {
		case ginQueryEmpty:
			continue
		case ginQueryFullScan:
			// Rows the index cannot enumerate (they carry no terms) may match: scan them all and let
			// the residual filter keep the right ones (gin.md §6 / §2.1). One such arm makes the
			// whole disjunction fall back.
			return store.ScanWithUnits(mask)
		}
		tree := q.tree
		if tree == nil {
			tree = ginTreeFromTerms(arm.strategy.isAny(), q.terms)
		}
		if gb.tagged {
			tree = tree.mapTerms(func(t []byte) []byte { return ginTaggedTerm(gb.tag, t) })
		}
		queries = append(queries, tree)
	}
	return db.ginGather(store, gb, queries, meter, mask, keysOnly)
}

// isAny reports whether the strategy's posting lists combine by union (mode ANY: `&&`, `?|`) rather
// than intersection (mode ALL).
func (s ginStrategy) isAny() bool {
	return s == ginOverlaps || s == ginJsonbExistsAny
}

// opclass is the bound column's GIN operator class.
func (gb *ginBoundPlan) opclass() ginOpclass {
	return ginOpclass{jsonb: gb.jsonb, elem: gb.elemType}
}

// ginGroupLookups: a skip-scan leaf group of at most this many candidates finds each candidate's run
// by its own lookup; a larger one scans the group's runs once (gin.md §6.2). An implementation
// choice: both find the same postings and the group's charges do not depend on it.
const ginGroupLookups = 4

// ginProbeGroupLookups is one leaf group's surviving candidates by a lookup per candidate — the first
// run at or after term ‖ c holds c if any run does — and (as a one-element slice, nil when none) the
// key of the run at or after the last candidate.
func ginProbeGroupLookups(istore *tableStore, term []byte, group [][]byte) (end, found [][]byte, err error) {
	var decodedRun []byte
	var keys [][]byte
	for _, c := range group {
		run, err := ginRunAtOrAfter(istore, term, c)
		if err != nil {
			return nil, nil, err
		}
		if run == nil {
			break // no later candidate has a run either
		}
		if !bytes.Equal(run, decodedRun) {
			if keys, err = ginDecodeRun(len(term), run); err != nil {
				return nil, nil, err
			}
			decodedRun = run
		}
		if _, ok := ginSearchKeys(keys, c); ok {
			found = append(found, c)
		}
		end = [][]byte{run}
	}
	return end, found, nil
}

// ginProbeGroupScan is one leaf group's surviving candidates by one scan of its runs, from
// term ‖ c_first through the first run past last (= term ‖ c_last), and that run's key.
func ginProbeGroupScan(istore *tableStore, term []byte, group [][]byte, first, last []byte) (end, found [][]byte, err error) {
	var runs [][]byte
	past := false
	b := ginPostingBound(term)
	b.lo = first
	if err := istore.ScanRange(b, func(k []byte, _ storedRow) (bool, error) {
		runs = append(runs, append([]byte{}, k...))
		past = bytes.Compare(k, last) > 0
		return !past, nil
	}); err != nil {
		return nil, nil, err
	}
	decodedAt := -1
	var keys [][]byte
	for _, c := range group {
		key := append(append([]byte{}, term...), c...)
		at := sort.Search(len(runs), func(k int) bool { return bytes.Compare(runs[k], key) >= 0 })
		if at == len(runs) {
			break
		}
		if at != decodedAt {
			if keys, err = ginDecodeRun(len(term), runs[at]); err != nil {
				return nil, nil, err
			}
			decodedAt = at
		}
		if _, ok := ginSearchKeys(keys, c); ok {
			found = append(found, c)
		}
	}
	if past {
		end = [][]byte{runs[len(runs)-1]}
	}
	return end, found, nil
}

// ginQueryKind is the outcome of a GIN opclass's extract_query_terms (gin.md §2/§6): distinct query
// terms to gather (sorted), a provably-empty bound, or the full-scan fallback for a query whose
// matches the index cannot enumerate.
type ginQueryKind int

const (
	ginQueryGather ginQueryKind = iota
	ginQueryEmpty
	ginQueryFullScan
)

type ginQuery struct {
	kind  ginQueryKind
	terms [][]byte
	// tree is a jsonpath strategy's AND/OR term tree (gin.md §2.2), set instead of terms.
	tree *ginTree
}

// ginQueryTerms is a resident GIN query value's terms under its operator class and strategy — what
// the gather reads, and what the planner prices from GIN term statistics (statistics.md §6.1).
func ginQueryTerms(opclass ginOpclass, strategy ginStrategy, qv Value) ginQuery {
	if opclass.jsonb {
		return jsonbQueryTerms(strategy, qv)
	}
	return arrayQueryTerms(strategy, opclass.elem, qv)
}

// arrayQueryTerms is the array_ops query terms (gin.md §2/§6). Each term is the element's
// order-preserving key encoding (gin.md §4) — the SAME bytes the entries carry, so a term doubles as
// its posting-list prefix. Encoding now lets us dedup distinct terms by bytes (a bijection:
// byte-dedup == value-dedup, byte-sort == value-sort) generically over every admitted element type.
func arrayQueryTerms(strategy ginStrategy, elem scalarType, qv Value) ginQuery {
	var terms [][]byte
	hasNull := false
	isEmpty := false
	if strategy == ginMember {
		// `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL c can equal no element,
		// so the bound is provably empty (gin.md §6). c may be a different numeric type than the
		// elements (an integer against decimal[], a decimal against i32[]); ginQueryTerm converts it to
		// the element type or reports that no element can equal it.
		if qv.Kind == ValNull {
			return ginQuery{kind: ginQueryEmpty}
		}
		t, ok := ginQueryTerm(elem, qv)
		if !ok {
			return ginQuery{kind: ginQueryEmpty} // c equals no element of the indexed type
		}
		terms = append(terms, t)
	} else {
		if qv.Kind != ValArray {
			return ginQuery{kind: ginQueryEmpty} // a NULL whole-array (or non-array) query → provably empty
		}
		// unmatchable: a query element no stored element can equal (an f64 not exactly an f32 against
		// an f32[] index — ginQueryTerm). It reaches no posting list, so @> and array = (which need
		// EVERY query element present) are provably empty; && skips it.
		unmatchable := false
		seen := make(map[string]bool)
		for _, el := range qv.arrayVal().Elements {
			if el.Kind == ValNull {
				hasNull = true // a NULL element carries no term
				continue
			}
			t, ok := ginQueryTerm(elem, el)
			if !ok {
				unmatchable = true
				continue
			}
			if !seen[string(t)] {
				seen[string(t)] = true
				terms = append(terms, t)
			}
		}
		isEmpty = len(qv.arrayVal().Elements) == 0
		if unmatchable && strategy != ginOverlaps {
			return ginQuery{kind: ginQueryEmpty}
		}
		slices.SortFunc(terms, bytes.Compare)
	}

	switch strategy {
	case ginContains:
		if isEmpty {
			// @> '{}': every non-NULL array contains the empty array — not derivable from the index;
			// fall back to the full scan (the residual filter keeps the right rows — gin.md §6).
			return ginQuery{kind: ginQueryFullScan}
		}
		if hasNull {
			return ginQuery{kind: ginQueryEmpty} // @> a query with a NULL element is never TRUE
		}
	case ginEqual:
		if len(terms) == 0 {
			// col = Q with NO non-NULL term — '{}' (isEmpty) or an all-NULL Q (hasNull, no non-NULL
			// element). The rows it matches ({}, {NULL}, …) carry NO index terms, so the index cannot
			// enumerate them: fall back to the full scan and let the residual = keep them (gin.md §6).
			// NOT a provably-empty bound — and a Q with ≥1 non-NULL element is NOT caught here (it
			// gathers, even when it also has a NULL element).
			return ginQuery{kind: ginQueryFullScan}
		}
	case ginOverlaps:
		if len(terms) == 0 {
			return ginQuery{kind: ginQueryEmpty} // && with no non-NULL term overlaps nothing
		}
	}
	return ginQuery{kind: ginQueryGather, terms: terms}
}

// ginGather is the shared inverted gather (gin.md §6 steps 2–4) over a non-empty, deduplicated,
// byte-sorted query term set: each term's posting list is the entry range [term, successor(term)) of
// the GIN tree — a term self-delimits (array_ops keys are fixed-width or terminated — text, §4.2;
// jsonb_ops terms end in a terminator or are a fixed 1–2 bytes, §4.1), so the storage key is the entry's suffix after
// the term's own length. The posting sets combine by mode (any → union, else intersection) into the
// candidate storage keys, ascending byte (= storage-key) order, each point-looked-up so lookups and
// emitted rows follow storage order exactly as a full scan (gin.md §6/§8). gin_entry (per posting
// entry visited) is charged on meter; the (pages, slabs) block is returned for the caller.
func (db *engine) ginGather(store *tableStore, gb *ginBoundPlan, arms []*ginTree, meter *costMeter, mask []bool, keysOnly bool) (out []entry, pages, slabs int, err error) {
	istore := db.lkpIndexStore(gb.nameKey)
	g := ginGatherer{istore: istore}
	var cand [][]byte
	if len(arms) == 1 {
		if cand, err = g.eval(arms[0]); err != nil {
			return nil, 0, 0, err
		}
	} else {
		// A disjunction: each arm's candidates in source order, unioned (gin.md §6.3).
		postings := make([][][]byte, 0, len(arms))
		for _, arm := range arms {
			keys, e := g.eval(arm)
			if e != nil {
				return nil, 0, 0, e
			}
			postings = append(postings, keys)
		}
		cand = unionPostings(postings)
	}
	meter.Charge(costs.GinEntry * int64(g.entries))
	pages = g.pages

	for _, key := range cand {
		if keysOnly {
			out = append(out, entry{Key: key})
			continue
		}
		row, ok, n, sl, e := store.GetWithUnits(key, mask)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += n
		slabs += sl
		if !ok {
			return nil, 0, 0, newError(DataCorrupted, "a GIN entry references a stored row")
		}
		out = append(out, entry{Key: key, Row: row})
	}
	return out, pages, slabs, nil
}

// jsonbQueryTerms is the jsonb_ops query terms (gin.md §2.1) of a resident query operand qv, with the
// degenerate shapes — `@>`: a NULL Q is provably empty, a term-less Q (`{}`, `[]`, …) falls back to
// the full scan, else its index terms ALL; `?`: a NULL k is empty, else its one key term; `?|`: a NULL
// K or no non-NULL key is empty, else the keys' terms ANY; `?&`: a NULL K is empty, no non-NULL key
// falls back to the full scan, else the keys' terms ALL. The gather is lossy; the residual operator
// rechecks every candidate.
func jsonbQueryTerms(strategy ginStrategy, qv Value) ginQuery {
	if qv.Kind == ValNull {
		return ginQuery{kind: ginQueryEmpty} // every strategy is strict: a NULL query operand matches nothing
	}
	var terms [][]byte
	switch strategy {
	case ginJsonPathExists, ginJsonPathMatch:
		// `@?` / `@@` (gin.md §2.2): the tree PostgreSQL extracts from the path, or the full scan when
		// nothing is extractable. The path text is a jsonpath value's canonical text, already compiled
		// at resolve; a term never depends on the environment's casing table.
		if qv.Kind != ValJsonPath {
			return ginQuery{kind: ginQueryEmpty}
		}
		path, err := compile(qv.str(), nil)
		if err != nil {
			return ginQuery{kind: ginQueryFullScan}
		}
		tree := jsonPathGinTree(&path, strategy == ginJsonPathExists)
		if tree == nil {
			return ginQuery{kind: ginQueryFullScan}
		}
		return ginQuery{kind: ginQueryGather, tree: tree}
	case ginJsonbContains:
		if qv.Kind != ValJsonb {
			return ginQuery{kind: ginQueryEmpty}
		}
		terms = ginJsonbIndexTerms(qv.jsonb())
		if len(terms) == 0 {
			return ginQuery{kind: ginQueryFullScan} // a term-less Q is contained by rows the index cannot enumerate
		}
	case ginJsonbExists:
		if qv.Kind != ValText {
			return ginQuery{kind: ginQueryEmpty}
		}
		terms = [][]byte{ginJsonbKeyTerm(qv.str())}
	default: // ginJsonbExistsAny / ginJsonbExistsAll — a text[] of keys
		if qv.Kind != ValArray {
			return ginQuery{kind: ginQueryEmpty}
		}
		for _, el := range qv.arrayVal().Elements {
			if el.Kind == ValNull {
				continue // a NULL key is skipped (json-sql-functions.md §1.2)
			}
			terms = append(terms, ginJsonbKeyTerm(el.str()))
		}
		if len(terms) == 0 {
			if strategy == ginJsonbExistsAny {
				return ginQuery{kind: ginQueryEmpty} // ?| with no non-NULL key matches nothing
			}
			return ginQuery{kind: ginQueryFullScan} // ?& with no non-NULL key is TRUE for every non-NULL document
		}
		slices.SortFunc(terms, bytes.Compare)
		terms = slices.CompactFunc(terms, bytes.Equal)
	}
	return ginQuery{kind: ginQueryGather, terms: terms}
}

// ginQueryTerm is the GIN term for one non-NULL query value against an index whose elements are
// elem — the element's order-preserving key encoding (gin.md §4), the same bytes the entries carry —
// or ok=false when the value can equal NO stored element. A float query value may be the OTHER float
// width (f64 = ANY(f32[]) compares by promoting the f32 elements, float.md §2), so it is first
// converted to the element width: an f32 value widens exactly; an f64 value narrows only when it is
// exactly an f32 (or NaN — every NaN is one value, float.md §3), else no f32 element can equal it.
// The float key canonicalizes -0 → +0 and every NaN (encoding.md §2.8), so term equality is exactly
// SQL float =. Integer and decimal compare by value (2 = 2.0), so a value of one against elements of
// the other is converted too (gin.md §2 Numeric terms): an integer becomes a decimal exactly; a
// decimal becomes an integer only when it is integral and in the element's range, else no element can
// equal it. An integer outside a narrower element's range equals no element either. Every other value
// takes the shared element term (ginElementTerm), which never collates or fails.
func ginQueryTerm(elem scalarType, v Value) ([]byte, bool) {
	switch {
	case elem == scalarDecimal && v.Kind == ValInt:
		return ginElementTerm(elem, DecimalValue(decimalFromInt64(v.Int))), true
	case elem.IsInteger() && v.Kind == ValDecimal:
		d := *v.decimal()
		if d.TruncToScale(0).CmpValue(d) != 0 {
			return nil, false
		}
		n, ok := d.ToInt64Round()
		if !ok || !elem.InRange(n) {
			return nil, false
		}
		return ginElementTerm(elem, IntValue(n)), true
	case elem.IsInteger() && v.Kind == ValInt && !elem.InRange(v.Int):
		return nil, false
	case elem == scalarFloat32 && v.Kind == ValFloat64:
		x := v.F64()
		narrowed := float32(x)
		if !math.IsNaN(x) && float64(narrowed) != x {
			return nil, false
		}
		return encodeFloat32Key(math.Float32bits(narrowed)), true
	case elem == scalarFloat64 && v.Kind == ValFloat32:
		return encodeFloat64Key(math.Float64bits(float64(v.F32()))), true
	}
	return ginElementTerm(elem, v), true
}

// gistBoundRows gathers a GiST-bounded scan's candidate rows (spec/design/gist.md §5). Evaluates the
// query operand, then DESCENDS the index's resident R-tree visiting only children
// consistent with the query, collecting candidate storage keys at the leaves; each candidate row is
// point-looked-up in storage-key order. The original &&/@> predicate stays the residual WHERE filter
// (always-recheck), so the result is exactly the full-scan result — the bound only narrows which rows
// are fetched. Returns the candidate (key, row) Entry pairs + the up-front (page_read, value_decompress)
// block (tree nodes visited + each candidate's lookup); gist_descent (per interior) is charged on meter
// directly. Degenerate constant queries (gist.md §5): a NULL Q and an empty && query match nothing; an
// empty @> query (col @> 'empty') matches every row → full-scan fallback (the empty bound is invisible
// to the overlap-descend).
func (db *engine) gistBoundRows(tableName string, gb *gistBoundPlan, query *rExpr, queryRow storedRow, env *evalEnv, meter *costMeter, mask []bool, keysOnly bool) (out []entry, pages, slabs int, err error) {
	store := db.lkpStore(tableName)
	if query == nil {
		return nil, 0, 0, nil
	}
	// Extracting the constant or once-per-outer sibling query is a planning step, NOT metered.
	qv, err := query.eval(queryRow, env, env.exec.session.scratchMeter())
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
		if keysOnly {
			out = append(out, entry{Key: key})
			continue
		}
		row, ok, n, sl, e := store.GetWithUnits(key, mask)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += n
		slabs += sl
		if !ok {
			return nil, 0, 0, newError(DataCorrupted, "a GiST entry references a stored row")
		}
		out = append(out, entry{Key: key, Row: row})
	}
	return out, pages, slabs, nil
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

// detectPKBound constructs the maximal equality prefix plus optional next-member range for a PK
// tuple. filters are walked independently as top-level AND chains (ordinary scans pass WHERE;
// index-nested-loop passes ON and WHERE). siblingColumns has the same meaning as asBoundTerm.
func (db *engine) detectPKBound(filters []*rExpr, rel scopeRel, siblingColumns columnRanges) *pkBoundPlan {
	pk := rel.table.PKIndices()
	if len(pk) == 0 {
		return nil
	}
	bp := &pkBoundPlan{memberCount: len(pk)}
	for _, ci := range pk {
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			break
		}
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			break
		}
		colColl := ""
		if coll != nil {
			colColl = coll.Name
		}
		var eqs []*rExpr
		var ranges []boundTerm
		var walk func(*rExpr)
		walk = func(e *rExpr) {
			if e == nil {
				return
			}
			if e.kind == reAnd {
				walk(e.lhs)
				walk(e.rhs)
				return
			}
			if t, ok := asBoundTerm(e, columnMatch(rel.offset+ci), ty, colColl, siblingColumns); ok {
				if t.op == opEq {
					eqs = append(eqs, t.src)
				} else {
					ranges = append(ranges, t)
				}
			}
		}
		for _, filter := range filters {
			walk(filter)
		}
		if len(eqs) > 0 {
			bp.eqCols = append(bp.eqCols, pkEqCol{name: rel.table.Columns[ci].Name, colType: ty, coll: coll, srcs: eqs, ranges: ranges})
			continue
		}
		if len(ranges) > 0 {
			bp.rangeName, bp.rangeType, bp.rangeColl, bp.rangeTerms = rel.table.Columns[ci].Name, ty, coll, ranges
		}
		break
	}
	if len(bp.eqCols) == 0 && bp.rangeTerms == nil {
		return nil
	}
	return bp
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
	case reNullIf:
		// NULLIF(a, b) is likewise a legal index expression (grammar.md §58).
		if a.caseDecimal != b.caseDecimal || a.nullifWidenFloat != b.nullifWidenFloat || len(a.sargs) != len(b.sargs) {
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
		return a.typmod == nil && b.typmod == nil && a.varchar == nil && b.varchar == nil && a.temporal == nil && b.temporal == nil &&
			a.result == b.result && rexprEqShifted(a.operand, b.operand, offset)
	case reNeg:
		return rexprEqShifted(a.operand, b.operand, offset)
	case reNot:
		return rexprEqShifted(a.operand, b.operand, offset)
	case reCasing:
		// lower(x)/upper(x) (spec/design/collation.md §16) resolve to a dedicated reCasing node —
		// NOT reScalarFunc — so an index on lower(email) (the headline expression-index shape)
		// matches ONLY if this arm is present. The fold is deterministic (the handle Environment's casing
		// regime, identical at index-build and query-eval), so the match is sound: same direction
		// + a matching argument.
		return a.casingUpper == b.casingUpper && rexprEqShifted(a.operand, b.operand, offset)
	case reCompare:
		// A comparison (status = 'active', amt > 0) is the canonical partial-index predicate shape
		// (indexes.md §9): same operator + same derived collation + structurally-equal operands.
		// `=`/`<>` are byte identity under every collation (collation.md §7), so their collation
		// never changes the value — only an ordering comparison must agree on it (a persisted index
		// expression derives by the storage rule, §1 "Function results").
		return a.op == b.op &&
			(a.op == opEq || a.op == opNe || collationsEqual(a.collation, b.collation)) &&
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

// asBoundTerm recognizes a single comparison conjunct: a comparison (=,<,<=,>,>=) whose one side
// matches the bound's key operand (a bare LOCAL column at key.col, or the resolved index
// expression — a correlated reOuterColumn is a different kind, so it never matches) and whose
// other side is a const-source of the key's own type (a promoted comparison — e.g. intpk = 2.5 → a
// reConstDecimal — does not match, so it stays residual). The op is flipped when the key is on the
// right.
type columnRange struct {
	start int
	end   int
}

func (r columnRange) contains(index int) bool {
	return index >= r.start && index < r.end
}

type columnRanges []columnRange

func (r columnRanges) contains(index int) bool {
	for _, span := range r {
		if index >= span.start && index < span.end {
			return true
		}
	}
	return false
}

func asBoundTerm(e *rExpr, key keyMatch, pkType scalarType, colColl string, siblingColumns columnRanges) (boundTerm, bool) {
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
	// column's form (sort key for a Full-collated column — buildKeyBound/indexBoundRows). One
	// exception: an EQUALITY on an expression key bounds under any comparison collation (one that
	// reaches here is C-keyed — a collated expression key stops the prefix first), since every
	// deterministic collation's `=` is byte identity (§7) — so lower(email) = $1 still seeks when
	// email is collated and the comparison inherits that collation through lower (§1 "Function
	// results").
	cmpColl := ""
	if e.collation != nil {
		cmpColl = e.collation.Name
	}
	if cmpColl != colColl && !(e.op == opEq && key.expr != nil) {
		return boundTerm{}, false
	}
	switch e.op {
	case opEq, opLt, opLe, opGt, opGe:
	default:
		return boundTerm{}, false
	}
	isKey := func(x *rExpr) bool { return key.matches(x) }
	switch {
	case isKey(e.lhs) && isConstSource(e.rhs, pkType, siblingColumns):
		return boundTerm{op: e.op, src: e.rhs}, true
	case isKey(e.rhs) && isConstSource(e.lhs, pkType, siblingColumns):
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
// siblingColumns opens the index-nested-loop door (cost.md §3 "JOIN"): a bare reColumn in the
// selected physical-left relation's global slot interval is a valid per-outer-row bound source.
// nil (the ordinary once-materialized bound) accepts only literals/params/outer references.
func isConstSource(e *rExpr, pkType scalarType, siblingColumns columnRanges) bool {
	switch e.kind {
	case reParam, reConstNull, reOuterColumn:
		return true
	case reColumn:
		return siblingColumns.contains(e.index)
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
	case reConstFloat32:
		return pkType == scalarFloat32
	case reConstFloat64:
		return pkType == scalarFloat64
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

// buildPKEqualityPrefix encodes the PK equality prefix once, including duplicate-source and
// same-column-range contradictions. didWiden preserves the deferred float-key fallback.
func (db *engine) buildPKEqualityPrefix(bp *pkBoundPlan, params []Value, outer []storedRow, left storedRow) (p []byte, widened keyBound, didWiden, empty bool) {
	for _, ec := range bp.eqCols {
		var agreed []byte
		for _, src := range ec.srcs {
			key, isNull, ok := encodeBoundKey(ec.colType, src, params, outer, ec.coll, left)
			if isNull {
				return nil, keyBound{}, false, true
			}
			if !ok {
				// Float bound encoding remains deferred. Preserve the old sound widening (and its
				// INL re-scan shape), retaining only any already-encoded leading tuple prefix.
				if ec.colType.IsFloat() {
					if len(p) == 0 {
						return nil, unboundedBound(), true, false
					}
					b := keyBound{lo: append([]byte(nil), p...), loInc: true, hi: prefixSuccessor(p), hiInc: false}
					return nil, b, true, boundEmpty(b)
				}
				return nil, keyBound{}, false, true
			}
			if agreed == nil {
				agreed = key
			} else if !bytes.Equal(agreed, key) {
				return nil, keyBound{}, false, true
			}
		}
		for _, term := range ec.ranges {
			key, isNull, ok := encodeBoundKey(ec.colType, term.src, params, outer, ec.coll, left)
			if isNull {
				return nil, keyBound{}, false, true
			}
			if !ok {
				continue
			}
			cmp := bytes.Compare(agreed, key)
			if (term.op == opGt && cmp <= 0) || (term.op == opGe && cmp < 0) ||
				(term.op == opLt && cmp >= 0) || (term.op == opLe && cmp > 0) {
				return nil, keyBound{}, false, true
			}
		}
		p = append(p, agreed...)
	}
	return p, keyBound{}, false, false
}

// buildKeyBound turns a PK tuple plan into a concrete storage-key range. Equality members append
// bare component encodings to P. A complete tuple is [P,P], a proper prefix is
// [P,prefixSuccessor(P)), and a next-member range tightens that prefix interval.
// outer carries the enclosing rows (innermost last) so a correlated reOuterColumn source resolves to
// the current outer row's value; it is nil for a top-level statement.
func (db *engine) buildKeyBound(bp *pkBoundPlan, params []Value, outer []storedRow, left storedRow) (keyBound, bool) {
	p, widened, didWiden, empty := db.buildPKEqualityPrefix(bp, params, outer, left)
	if empty {
		return keyBound{}, true
	}
	if didWiden {
		return widened, false
	}
	if len(bp.eqCols) == bp.memberCount {
		return keyBound{lo: p, loInc: true, hi: append([]byte(nil), p...), hiInc: true}, false
	}
	b := keyBound{lo: append([]byte(nil), p...), loInc: true, hi: prefixSuccessor(p), hiInc: false}
	if len(p) == 0 {
		b.lo = nil
	}
	for _, t := range bp.rangeTerms {
		key, isNull, ok := encodeBoundKey(bp.rangeType, t.src, params, outer, bp.rangeColl, left)
		if isNull {
			return keyBound{}, true
		}
		if !ok {
			continue
		}
		endpoint := append(append([]byte(nil), p...), key...)
		switch t.op {
		case opGt:
			next := prefixSuccessor(endpoint)
			if next == nil {
				return keyBound{}, true
			}
			b = intersectLo(b, next, true)
		case opGe:
			b = intersectLo(b, endpoint, true)
		case opLt:
			b = intersectHi(b, endpoint, false)
		case opLe:
			if next := prefixSuccessor(endpoint); next != nil {
				b = intersectHi(b, next, false)
			}
		}
	}
	if boundEmpty(b) {
		return keyBound{}, true
	}
	return b, false
}

// buildCompletePKPoint resolves a full-PK equality plan to one storage key. It shares the equality
// encoder with the general range path but does not duplicate the key into [lo, hi].
func (db *engine) buildCompletePKPoint(bp *pkBoundPlan, params []Value, outer []storedRow, left storedRow) (key []byte, point, empty bool) {
	if len(bp.eqCols) != bp.memberCount || len(bp.rangeTerms) != 0 {
		return nil, false, false
	}
	key, _, widened, empty := db.buildPKEqualityPrefix(bp, params, outer, left)
	if empty {
		return nil, true, true
	}
	if !widened {
		return key, true, false
	}
	return nil, false, false
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
	case reConstFloat32:
		return encodeFloat32Key(math.Float32bits(float32(src.cFloat))), false, true
	case reConstFloat64:
		return encodeFloat64Key(math.Float64bits(src.cFloat)), false, true
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
		if pkType == scalarFloat32 && v.Kind == ValFloat32 {
			return encodeFloat32Key(math.Float32bits(v.F32())), false, true
		}
		if pkType == scalarFloat64 && v.Kind == ValFloat64 {
			return encodeFloat64Key(math.Float64bits(v.F64())), false, true
		}
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
// execStreamingScan executes the bounded streaming scan path (spec/design/cost.md §3): full or
// contiguous-PK scans, canonical PK/index interval sets, and compatible ordered-index scans stop at
// the LIMIT/OFFSET window. GIN/GiST complete their candidate gather, then stop table point-lookups at
// that window. Only started interval blocks are charged; an opclass gather is charged in full.
// streamingScanEligible reports whether plan is the single-table, no-blocking-operator STREAMING SCAN
// shape (spec/design/cost.md §3, streaming.md §4) — a single relation, no join/aggregate/window, an
// output order the chosen bound already yields, and a real table store (not an SRF / CTE / derived
// source). Without ORDER BY, LIMIT observes the chosen access path's existing deterministic order.
// With ORDER BY, PK/PK-interval bounds must preserve PK order, or an ordered-index bound/set must
// walk the exact index that satisfies the order.
func streamingScanEligible(plan *selectPlan) bool {
	if len(plan.rels) != 1 || len(plan.joins) != 0 || plan.isAgg || plan.hasWindow ||
		plan.rels[0].srf != nil || plan.rels[0].cte != nil || plan.rels[0].derived != nil {
		return false
	}
	sb := plan.phys.relBounds[0]
	if len(plan.order) == 0 {
		return !plan.distinct && plan.limit != nil
	}
	if plan.phys.pkOrdered {
		return sb == nil || sb.pk != nil || sb.pkSet != nil || sb.gin != nil || sb.gist != nil
	}
	return plan.phys.indexOrder != nil && indexOrderCompatibleBound(plan.phys.indexOrder, sb) && sb != nil
}

// pullStreamingScanEligible is the narrower gate for the Query API's direct storeScan cursor. The
// generalized bound streams still execute lazily through bufferedScanCursor on first pull, but that
// older cursor understands only a full/contiguous-PK scan.
func pullStreamingScanEligible(plan *selectPlan) bool {
	if !streamingScanEligible(plan) {
		return false
	}
	sb := plan.phys.relBounds[0]
	return sb == nil || sb.pk != nil
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
	nameKey string // the index store's key — the exact index name
	pkWidth int    // the fixed PK-suffix byte width to peel off the END of each index entry key
}

// orderSatisfiedByIndex reports whether a single base relation's ORDER BY is satisfied by walking one
// of its B-tree SECONDARY indexes in key order (cost.md §3 "secondary-index order"), and which index.
// The index store holds its entries in (indexed columns, storage key) order, so a forward walk
// delivers rows in ORDER BY <indexed columns> ASC NULLS LAST order, ties broken by the PK — exactly
// the eager stable sort's tie-break. Returns non-nil iff the ORDER BY keys are EXACTLY a B-tree
// index's columns (same count, same columns in key order), each with the index element's declared
// direction and NULL placement (indexes.md §11 — the §2.16 ordered slot realizes it) and sorting by the column's stored key collation (Skewed/unresolvable → refuse, §12), AND
// the table's PK is fixed-width. The exact-match requirement is load-bearing: a strict prefix of a
// multi-column index would tie-break by the remaining index columns, not the PK.
func (db *engine) orderSatisfiedByIndexes(table *catTable, offset int, order []orderSlot, filter *rExpr) []indexOrderPlan {
	pkWidth, ok := pkStorageWidth(table)
	if !ok {
		return nil
	}
	var matches []indexOrderPlan
	for _, idx := range table.Indexes {
		if idx.Kind != indexBtree {
			continue // only an ordered B-tree realizes the column order (GIN/GiST do not)
		}
		// A PARTIAL index holds only its qualifying rows (indexes.md §9), so walking it for order
		// is sound only when the WHERE implies its predicate (§9.1). The caller is a single-relation
		// scan, so no row is NULL-extended.
		if idx.Predicate != nil {
			if filter == nil || !db.partialIndexImplied(filter, table, offset, idx) {
				continue
			}
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
		matchesOrder := true
		for i, o := range order {
			// A forward walk realizes each key element's declared order (indexes.md §11): the ORDER BY
			// key's direction and effective NULL placement must both match.
			if ko := idx.keyOrder(i); o.descending != ko.Desc || o.nullsFirst != ko.NullsFirst {
				matchesOrder = false
				break
			}
			if o.idx != offset+cols[i] {
				matchesOrder = false
				break
			}
			coll, push := db.keyCollationCtx(table.Columns[cols[i]])
			if !push { // Skewed / unresolvable — never walked for order (§12)
				matchesOrder = false
				break
			}
			if coll == nil {
				if o.collation != nil {
					matchesOrder = false
					break
				}
			} else if o.collation == nil || o.collation.Name != coll.Name {
				matchesOrder = false
				break
			}
		}
		if matchesOrder {
			matches = append(matches, indexOrderPlan{nameKey: idx.Name, pkWidth: pkWidth})
		}
	}
	sort.SliceStable(matches, func(i, j int) bool {
		return bytes.Compare([]byte(matches[i].nameKey), []byte(matches[j].nameKey)) < 0
	})
	return matches
}

func (db *engine) orderSatisfiedByIndex(table *catTable, offset int, order []orderSlot, filter *rExpr) *indexOrderPlan {
	matches := db.orderSatisfiedByIndexes(table, offset, order, filter)
	if len(matches) == 0 {
		return nil
	}
	return &matches[0]
}

// ginGatherer accumulates the posting reads of one GIN gather and their units: page_read nodes and
// gin_entry entries (gin.md §6 Cost).
type ginGatherer struct {
	istore  *tableStore
	pages   int
	entries int
}

// ginPostingBound is the posting range of term: every entry term ‖ storage_key (gin.md §4).
func ginPostingBound(term []byte) keyBound {
	return keyBound{lo: term, loInc: true, hi: prefixSuccessor(term), hiInc: false}
}

// eval is the candidates of a query tree (gin.md §2.2/§6), ascending. A term is its whole posting
// list. An Or unions its leaves' lists (in byte order), then its children's candidates (in order). An
// And intersects its leaves by the skip-scan (§6.2) — or, with no leaf, starts from its first child —
// then intersects each following child's candidates in order, stopping once none survives. Mode ALL
// is an And of terms and mode ANY an Or, so `= ANY` and `?` (one term) read that lone list, array `=`
// gathers the same superset as `@>` (the residual makes it exact), and the jsonb_ops gathers are
// supersets the residual rechecks.
func (g *ginGatherer) eval(t *ginTree) ([][]byte, error) {
	switch t.kind {
	case ginTreeTerm:
		return g.postingList(t.term)
	case ginTreeOr:
		postings := make([][][]byte, 0, len(t.leaves)+len(t.rest))
		for _, term := range t.leaves {
			keys, e := g.postingList(term)
			if e != nil {
				return nil, e
			}
			postings = append(postings, keys)
		}
		for _, child := range t.rest {
			keys, e := g.eval(child)
			if e != nil {
				return nil, e
			}
			postings = append(postings, keys)
		}
		return unionPostings(postings), nil
	}
	rest := t.rest
	var cand [][]byte
	var err error
	if len(t.leaves) > 0 {
		cand, err = g.intersect(t.leaves)
	} else if len(rest) > 0 {
		cand, err = g.eval(rest[0])
		rest = rest[1:]
	}
	if err != nil {
		return nil, err
	}
	for _, child := range rest {
		if len(cand) == 0 {
			break
		}
		found, e := g.eval(child)
		if e != nil {
			return nil, e
		}
		in := make(map[string]bool, len(found))
		for _, k := range found {
			in[string(k)] = true
		}
		kept := cand[:0:0]
		for _, k := range cand {
			if in[string(k)] {
				kept = append(kept, k)
			}
		}
		cand = kept
	}
	return cand, nil
}

// postingList reads term's whole posting list (a prefix range scan), charging its overlap nodes and
// entries, and returns the storage keys (the suffix after the term) in ascending order.
func (g *ginGatherer) postingList(term []byte) ([][]byte, error) {
	es, p, _, err := g.istore.RangeScanWithUnits(ginPostingBound(term), nil)
	if err != nil {
		return nil, err
	}
	g.pages += p
	return g.decodeRuns(term, es)
}

// decodeRuns is the storage keys of a term's run records (gin.md §4.5), charging one gin_entry each.
func (g *ginGatherer) decodeRuns(term []byte, es []entry) ([][]byte, error) {
	var keys [][]byte
	for _, e := range es {
		ks, err := ginDecodeRun(len(term), e.Key)
		if err != nil {
			return nil, err
		}
		keys = append(keys, ks...)
	}
	g.entries += len(keys)
	return keys, nil
}

// intersect is the ALL-mode skip-scan intersection (gin.md §6.2). The terms are ordered by their
// posting range's skeleton overlap count; a term whose range lies in one leaf is peeked — its run
// records read, charging the path — and ordered by its run count and bytes, ties by term bytes. The
// first list is read whole, and each following term is probed at the surviving candidates in leaf
// groups — runs of consecutive candidates whose probe keys fall in one leaf — each reading the runs
// from its first probe key through the run past its last, charging the nodes that range overlaps and
// one gin_entry per step of the cheaper leaf-local match. Stops once no candidate survives.
func (g *ginGatherer) intersect(terms [][]byte) ([][]byte, error) {
	if len(terms) == 0 {
		return nil, nil
	}
	if len(terms) == 1 {
		return g.postingList(terms[0])
	}
	height := g.istore.Height()
	type spanned struct {
		span, runs, bytes int
		term              []byte
		peeked            []entry
		wasPeeked         bool
	}
	ordered := make([]spanned, len(terms))
	for i, t := range terms {
		o := spanned{span: g.istore.OverlapNodeCount(ginPostingBound(t)), term: t}
		if o.span <= height {
			es, p, _, err := g.istore.RangeScanWithUnits(ginPostingBound(t), nil)
			if err != nil {
				return nil, err
			}
			g.pages += p
			o.runs, o.peeked, o.wasPeeked = len(es), es, true
			for _, e := range es {
				o.bytes += len(e.Key)
			}
		} else {
			o.runs, o.bytes = math.MaxInt, math.MaxInt
		}
		ordered[i] = o
	}
	slices.SortFunc(ordered, func(a, b spanned) int {
		switch {
		case a.span != b.span:
			return cmp.Compare(a.span, b.span)
		case a.runs != b.runs:
			return cmp.Compare(a.runs, b.runs)
		case a.bytes != b.bytes:
			return cmp.Compare(a.bytes, b.bytes)
		}
		return bytes.Compare(a.term, b.term)
	})
	var cand [][]byte
	var err error
	if ordered[0].wasPeeked {
		cand, err = g.decodeRuns(ordered[0].term, ordered[0].peeked)
	} else {
		cand, err = g.postingList(ordered[0].term)
	}
	if err != nil {
		return nil, err
	}
	probe := func(term, key []byte) []byte {
		return append(append(make([]byte, 0, len(term)+len(key)), term...), key...)
	}
	span := func(lo, hi []byte) keyBound { return keyBound{lo: lo, loInc: true, hi: hi, hiInc: true} }
	for _, o := range ordered[1:] {
		if len(cand) == 0 {
			break
		}
		term := o.term
		var survivors [][]byte
		for i := 0; i < len(cand); {
			first := probe(term, cand[i])
			j := i
			for j+1 < len(cand) && g.istore.OverlapNodeCount(span(first, probe(term, cand[j+1]))) == height {
				j++
			}
			// One leaf: the runs from term ‖ c_i through the run holding (or following) c_j — the first
			// run past term ‖ c_j (gin.md §4.5/§6.2). The group charges the nodes that range overlaps
			// (its root-to-leaf path when the run is in the same leaf), then the cheaper leaf-local match
			// — a probe per candidate, or a merge with the runs' storage keys — one gin_entry per step.
			last := probe(term, cand[j])
			group := cand[i : j+1]
			// The run past term ‖ c_j ends the charged range; membership is found by a lookup per
			// candidate when the group is small, else by one scan of the group's runs — the same
			// answer and charges either way.
			var end, found [][]byte
			var err error
			if len(group) <= ginGroupLookups {
				end, found, err = ginProbeGroupLookups(g.istore, term, group)
			} else {
				end, found, err = ginProbeGroupScan(g.istore, term, group, first, last)
			}
			if err != nil {
				return nil, err
			}
			stop := last
			if end != nil {
				stop = end[0]
			}
			g.pages += g.istore.OverlapNodeCount(span(first, stop))
			g.entries += len(group)
			survivors = append(survivors, found...)
			i = j + 1
		}
		cand = survivors
	}
	return cand, nil
}
