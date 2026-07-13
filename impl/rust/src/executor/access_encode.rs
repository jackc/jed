//! Access-path filter analysis and key/index encoding (mirrors impl/go access_path.go + the index/key
//! encoders in dml.go/store_encode.go): the ScanBound/FkProbe/ScanSource impls, the WHERE-filter bound
//! detection (detect_scan_bound/detect_pk_bound/inventory_scan_candidates, order_satisfied_by_*),
//! the secondary-index entry encoders (index_entry_key/gist_entries/gin_entries), and the order-preserving
//! key encoders (encode_key_value/encode_typed_key/encode_array_key, encode_bound_key).

use super::*;

impl ScanBound {
    /// Whether this bound needs the general eager materialize path (`materialize_rel` / the DML
    /// scan) rather than a single-contiguous-range fast path (streaming scan, columnar project,
    /// vectorized aggregate, streaming sort, join top-N). True for a second-tree gather
    /// (index / GIN / GiST) and for a canonical interval set (`PkSet` / `IndexSet`); false for a plain PK contiguous bound (which every fast
    /// path handles via a single `build_key_bound`). Every single-table fast-path gate consults
    /// this so interval-set bounds are interpreted in exactly ONE place (`materialize_rel`), never
    /// silently dropped to a full scan by a fast path that only understands `Pk`.
    pub(crate) fn needs_eager_scan(&self) -> bool {
        matches!(
            self,
            ScanBound::Index(_)
                | ScanBound::Gin(_)
                | ScanBound::Gist(_)
                | ScanBound::PkSet(_)
                | ScanBound::IndexSet(_)
        )
    }
}

/// The plan-time result for one eligible GiST index (spec/design/gist.md §5): its operator strategy
/// and the column's global scope index. The inventory owns one plan per eligible index; the selector
/// chooses among them. Like [`GinBound`], the constant query operand is NOT stored (re-found in
/// `plan.filter` at exec time by `gist_match`). No element type is carried: the gather descends the
/// resident R-tree (gist.md §4.1), whose bounds are already decoded.
pub(crate) struct GistBound {
    /// The index store's key — the lowercased index name (its resident R-tree lives under this key).
    pub(crate) name_key: String,
    pub(crate) strategy: crate::gist::GistStrategy,
    /// The GiST-indexed column's global scope index (`rel.offset + ci`).
    pub(crate) col_global: usize,
    /// `Some(scalar)` for the scalar `=` opclass (GX2): the column's scalar type, so `gist_bound_rows`
    /// can encode the equality constant to its order-preserving key bytes. `None` for `range_ops`,
    /// whose `&&`/`@>` query is a range constant the resident R-tree compares directly.
    pub(crate) scalar_type: Option<ScalarType>,
}

/// Which array operator a GIN bound accelerates (spec/design/gin.md §6): `@>` (contains, mode
/// ALL → posting-list intersection), `&&` (overlaps, mode ANY → posting-list union), or
/// `= ANY` (membership — `c = ANY(col)`, the single-term `@>` reduction: one scalar term, mode
/// ALL → its lone posting list).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GinStrategy {
    Contains,
    Overlaps,
    /// `c = ANY(col)` — `c` is a constant SCALAR (not an array); its single term is gathered like
    /// a one-element `@>`. The query operand recovered by `gin_match` is the scalar `c`.
    Member,
    /// `col = Q` — exact array equality. The query operand is the constant array `Q`; its distinct
    /// non-NULL elements gather the SAME candidate superset as `@> Q` (equal arrays have identical
    /// element multisets, so `col = Q` ⟹ `col @> Q`), and the residual `=` filter makes it exact.
    /// Unlike `Contains`, a NULL ELEMENT of `Q` does not empty the bound; and a `Q` with no non-NULL
    /// element (`'{}'`/all-NULL) falls back to the full scan, not a provably-empty bound (gin.md §6).
    Equal,
}

/// The plan-time result for one eligible GIN index (spec/design/gin.md §6): its array **element**
/// type (for `encode_element` — the term bytes), operator strategy, and column's global scope index.
/// The inventory owns one plan per eligible index; the selector chooses among them. The constant
/// query `Q` is NOT stored (`RExpr` is not `Clone`); it is re-found in `plan.filter` at exec time by
/// `gin_match` and evaluated there.
pub(crate) struct GinBound {
    /// The index store's key — the lowercased index name.
    pub(crate) name_key: String,
    /// The array element type, whose key encoding produces each term's bytes.
    pub(crate) elem_type: ScalarType,
    pub(crate) strategy: GinStrategy,
    /// The GIN-indexed column's global scope index (`rel.offset + ci`).
    pub(crate) col_global: usize,
}

/// One column of an index access predicate's equality prefix (indexes.md §5.1): the column's
/// storage type, its key collation (`Some(coll)` only for a `Full`-collated text column), and every
/// equality const-source bound to it. At exec time the sources must agree on one value (else the
/// bound is provably empty). A collated column encodes its probe via the UCA sort key
/// (encoding.md §2.12) to match the index's stored key form (collation.md §8).
pub(crate) struct IndexEqCol {
    pub(crate) col_type: ScalarType,
    pub(crate) coll: Option<std::sync::Arc<Collation>>,
    pub(crate) srcs: Vec<BoundSrc>,
}

/// The optional trailing range of an index access predicate (indexes.md §5.1): a range on the key
/// column immediately after the equality prefix. Its column is fixed-width (never collated).
pub(crate) struct IndexRange {
    pub(crate) col_type: ScalarType,
    pub(crate) terms: Vec<BoundTerm>,
}

/// The plan-time result for one eligible ordered index (indexes.md §5.1): a maximal EQUALITY PREFIX
/// on the leading key columns (`eq_cols`) plus an OPTIONAL RANGE on the next column (`range`). The
/// inventory owns one plan per eligible index; the selector chooses among them. At exec time
/// `build_index_bound` turns these into a concrete index-key range: the equality prefix bytes
/// P = concatenated present slots, then the range (if any) intersected relative to P.
/// `suffix_types` are the types of the index columns AFTER the equality prefix (`columns[eq..]`) —
/// the range column (if any) plus every trailing column — each FIXED-WIDTH so an admitted entry's
/// row-key suffix is recovered by width-skipping them past P.
pub(crate) struct IndexBound {
    /// The index store's key — the lowercased index name.
    pub(crate) name_key: String,
    pub(crate) eq_cols: Vec<IndexEqCol>,
    pub(crate) range: Option<IndexRange>,
    pub(crate) suffix_types: Vec<ScalarType>,
}

/// The outcome of encoding a const-source into the PK key space.
pub(crate) enum BoundKey {
    /// A NULL const — the comparison is 3VL-unknown, so the range is provably empty.
    Null,
    /// An integer value outside the PK type's range — no key can equal it, so drop this half-bound.
    OutOfRange,
    Key(Vec<u8>),
}

/// Construct an index access predicate for `idx` over `rel` (indexes.md §5.1): a maximal EQUALITY
/// PREFIX on the leading key columns plus an OPTIONAL RANGE on the next column. It walks the index's
/// key columns in key order against the WHERE AND-chain, consuming a column with an agreed equality
/// conjunct into the prefix and stopping at the first column that has no equality (taking its range
/// conjuncts, if any, as the trailing range). Returns `None` for a non-B-tree index, a `Skewed`
/// collated bound column (whose stored keys are at the file's pinned version — collation.md §12), no
/// bound at all, or an ineligible suffix (a column after the equality prefix that is not a
/// fixed-width scalar — the width-based key-suffix skip needs it). `sibling_cutoff` opens the
/// index-nested-loop door (`Some(cut)` admits a bare sibling `Column(g)` with `g < cut` as a bound
/// source, resolved per outer row); `None` is the ordinary once-materialized bound.
pub(crate) fn build_index_access_predicate(
    filter: &RExpr,
    rel: &ScopeRel,
    idx: &IndexDef,
    sibling_cutoff: Option<usize>,
    catalog: &Engine,
) -> Option<IndexBound> {
    if idx.kind != IndexKind::Btree {
        return None;
    }
    // Resolve the index's key elements (column ordinals + resolved expression keys). A resolution
    // failure yields no bound (a full scan — always sound). indexes.md §5.
    let rindex = catalog.resolve_index(rel.table, idx).ok()?;
    // A PARTIAL index holds only its qualifying rows (indexes.md §9), so it is usable ONLY when the
    // query's WHERE implies the index predicate. jed's test is syntactic (PG's, not a prover): the
    // WHERE AND-chain must contain a conjunct STRUCTURALLY EQUAL to the resolved predicate. A miss
    // yields no bound — a correct full scan. (The resolved predicate is in table-local column coords;
    // a WHERE conjunct is global, so it is matched shifted by `rel.offset`.)
    if let Some(pred) = &rindex.predicate {
        if !filter_implies_predicate(filter, pred, rel.offset) {
            return None;
        }
    }
    let mut eq_cols: Vec<IndexEqCol> = Vec::new();
    let mut range: Option<IndexRange> = None;
    for key in &rindex.keys {
        // Each key element yields (its scalar key type, its key collation, the matcher against a
        // WHERE conjunct operand). A non-scalar / skewed element stops the prefix.
        let (ty, coll, matcher): (ScalarType, Option<std::sync::Arc<Collation>>, KeyMatch) =
            match key {
                ResolvedKey::Column(ci) => {
                    let Some(ty) = rel.table.columns[*ci].ty.as_scalar() else {
                        break; // a range/array/composite column cannot be seeked
                    };
                    // Collation.md §8/§12: a `Skewed` collated column refuses the bound (its stored
                    // keys are wrong for the loaded bundle) — stop the prefix. `C`/`Full` admissible.
                    let Some(coll) = key_collation_ctx(catalog, &rel.table.columns[*ci]) else {
                        break;
                    };
                    (ty, coll, KeyMatch::Column(rel.offset + *ci))
                }
                ResolvedKey::Expr(rexpr, ety, ecoll) => {
                    // An expression key seeks only when its result is a scalar and its collation is
                    // `C` (the common `lower(email)` shape). A collated-expression bound is a
                    // deferred follow-on (§5). Match a WHERE operand structurally against the key.
                    let Some(ty) = ety.as_scalar() else { break };
                    if ecoll.is_some() {
                        break;
                    }
                    (ty, None, KeyMatch::Expr(rexpr, rel.offset))
                }
            };
        let mut terms = Vec::new();
        collect_bound_terms(
            filter,
            &matcher,
            ty,
            coll.as_ref().map(|c| c.name.as_str()),
            sibling_cutoff,
            &mut terms,
        );
        let (eqs, ranges): (Vec<BoundTerm>, Vec<BoundTerm>) =
            terms.into_iter().partition(|t| matches!(t.op, CmpOp::Eq));
        if !eqs.is_empty() {
            eq_cols.push(IndexEqCol {
                col_type: ty,
                coll,
                srcs: eqs.into_iter().map(|t| t.src).collect(),
            });
            continue; // extend the equality prefix
        }
        if !ranges.is_empty() {
            range = Some(IndexRange {
                col_type: ty,
                terms: ranges,
            });
        }
        break; // first non-equality element ends the prefix (with or without a trailing range)
    }
    if eq_cols.is_empty() && range.is_none() {
        return None; // nothing bound
    }
    // Eligibility: every key element from the range element onward (`keys[eq_cols.len()..]`) is
    // width-skipped past the known equality prefix to recover the storage key, so each must be a
    // fixed-width scalar (a column's type, or an expression's result type). The equality-prefix
    // elements may be any width — their slots are matched as the known prefix bytes.
    let mut suffix_types = Vec::with_capacity(rindex.keys.len() - eq_cols.len());
    for key in &rindex.keys[eq_cols.len()..] {
        let s = match key {
            ResolvedKey::Column(ci) => rel.table.columns[*ci].ty.as_scalar()?,
            ResolvedKey::Expr(_, ety, _) => ety.as_scalar()?,
        };
        if !s.is_fixed_width() {
            return None;
        }
        suffix_types.push(s);
    }
    Some(IndexBound {
        name_key: idx.name.to_ascii_lowercase(),
        eq_cols,
        range,
        suffix_types,
    })
}

/// Canonical access-path rank from estimator.toml. Keep declaration order byte-identical to the
/// shared fact: P4 compares estimated cost first, then this rank. P3 uses it only to sort inventory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ScanCandidateKind {
    Pk,
    Btree,
    Gist,
    Gin,
    PkInterval,
    IndexInterval,
    Full,
}

/// Collision-free physical identity of one base-relation access candidate. `index_name` is the
/// lowercased catalog name for index-bearing paths and empty for PK/PK-interval/full paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScanCandidateIdentity {
    kind: ScanCandidateKind,
    index_name: String,
}

impl std::fmt::Display for ScanCandidateIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ScanCandidateKind::Pk => f.write_str("pk"),
            ScanCandidateKind::Btree => write!(f, "btree:{}", self.index_name),
            ScanCandidateKind::Gist => write!(f, "gist:{}", self.index_name),
            ScanCandidateKind::Gin => write!(f, "gin:{}", self.index_name),
            ScanCandidateKind::PkInterval => f.write_str("pk_interval"),
            ScanCandidateKind::IndexInterval => {
                write!(f, "index_interval:{}", self.index_name)
            }
            ScanCandidateKind::Full => f.write_str("full"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScanOrderCapability {
    /// Table storage-key order. Full/PK/PK-set scans walk it directly; GIN/GiST gathers normalize to
    /// it. The current executor supports both forward and reverse traversal.
    StorageKey { reversible: bool },
    /// One ordered B-tree's key order. The current ordered-index path walks only forward.
    IndexKey {
        index_name: String,
        reversible: bool,
    },
}

/// One legal base-relation access path. `bound = None` only for the explicit full scan. `residual`
/// is always the complete WHERE (or `None` when there is no WHERE), because every access predicate
/// is only a narrowing superset and execution retains the full recheck.
pub(crate) struct ScanCandidate<'a> {
    identity: ScanCandidateIdentity,
    bound: Option<ScanBound>,
    scan_order: ScanOrderCapability,
    residual: Option<&'a RExpr>,
}

fn storage_order_candidate<'a>(
    kind: ScanCandidateKind,
    name: String,
    bound: Option<ScanBound>,
    filter: Option<&'a RExpr>,
) -> ScanCandidate<'a> {
    ScanCandidate {
        identity: ScanCandidateIdentity {
            kind,
            index_name: name,
        },
        bound,
        scan_order: ScanOrderCapability::StorageKey { reversible: true },
        residual: filter,
    }
}

fn index_order_candidate<'a>(
    kind: ScanCandidateKind,
    name: String,
    bound: ScanBound,
    filter: Option<&'a RExpr>,
) -> ScanCandidate<'a> {
    ScanCandidate {
        identity: ScanCandidateIdentity {
            kind,
            index_name: name.clone(),
        },
        bound: Some(bound),
        scan_order: ScanOrderCapability::IndexKey {
            index_name: name,
            reversible: false,
        },
        residual: filter,
    }
}

/// Consumer-specific eligibility/precedence for the behavior-neutral LEGACY selector. Inventory is
/// policy-free and complete. Mutation tries GIN before GiST; SELECT tries GiST before GIN.
#[derive(Clone, Copy)]
pub(crate) struct ScanBoundPolicy {
    ordered_index: bool,
    index_set: bool,
    gist_before_gin: bool,
}

pub(crate) const SELECT_SCAN_BOUND_POLICY: ScanBoundPolicy = ScanBoundPolicy {
    ordered_index: true,
    index_set: true,
    gist_before_gin: true,
};

pub(crate) const MUTATION_SCAN_BOUND_POLICY: ScanBoundPolicy = ScanBoundPolicy {
    ordered_index: true,
    index_set: true,
    gist_before_gin: false,
};

/// Pick one SELECT relation's scan bound (cost.md §3; indexes.md §5). This is the SELECT-policy
/// wrapper over the shared inventory plus behavior-neutral legacy selector.
pub(crate) fn detect_scan_bound(
    filter: &RExpr,
    rel: &ScopeRel,
    catalog: &Engine,
) -> Option<ScanBound> {
    detect_scan_bound_with_policy(filter, rel, catalog, SELECT_SCAN_BOUND_POLICY)
}

/// Enumerate EVERY legal base access path in estimator.toml's canonical rank/name order. It never
/// selects. A host-attached relation has only the full candidate because bounded execution still
/// resolves its index stores through the unscoped funnel.
pub(crate) fn inventory_scan_candidates<'a>(
    filter: Option<&'a RExpr>,
    rel: &ScopeRel,
    catalog: &Engine,
) -> Vec<ScanCandidate<'a>> {
    let full = || storage_order_candidate(ScanCandidateKind::Full, String::new(), None, filter);
    let Some(filter_expr) = filter else {
        return vec![full()];
    };
    if rel.is_attachment() {
        return vec![full()];
    }
    let mut candidates = Vec::with_capacity(2 + rel.table.indexes.len() * 2);
    if let Some(bound) = detect_pk_bound(&[filter_expr], rel, None, catalog) {
        candidates.push(storage_order_candidate(
            ScanCandidateKind::Pk,
            String::new(),
            Some(ScanBound::Pk(bound)),
            filter,
        ));
    }
    for idx in &rel.table.indexes {
        if let Some(bound) = build_index_access_predicate(filter_expr, rel, idx, None, catalog) {
            let name = bound.name_key.clone();
            candidates.push(index_order_candidate(
                ScanCandidateKind::Btree,
                name,
                ScanBound::Index(bound),
                filter,
            ));
        }
    }
    for idx in &rel.table.indexes {
        if let Some(bound) =
            build_gist_bound_for_index(filter_expr, idx, &rel.table.columns, rel.offset)
        {
            let name = bound.name_key.clone();
            candidates.push(storage_order_candidate(
                ScanCandidateKind::Gist,
                name,
                Some(ScanBound::Gist(bound)),
                filter,
            ));
        }
    }
    for idx in &rel.table.indexes {
        if let Some(bound) =
            build_gin_bound_for_index(filter_expr, idx, &rel.table.columns, rel.offset)
        {
            let name = bound.name_key.clone();
            candidates.push(storage_order_candidate(
                ScanCandidateKind::Gin,
                name,
                Some(ScanBound::Gin(bound)),
                filter,
            ));
        }
    }
    let pk_intervals = rel.table.primary_key_index().and_then(|pk_local| {
        let sty = rel.table.columns[pk_local].ty.as_scalar()?;
        let coll = key_collation_ctx(catalog, &rel.table.columns[pk_local])?;
        let (specs, clip) =
            detect_interval_set(filter_expr, rel.offset + pk_local, sty, coll.as_deref())?;
        Some(PkKeySet {
            pk_type: sty,
            coll,
            specs,
            clip,
        })
    });
    if let Some(bound) = pk_intervals {
        candidates.push(storage_order_candidate(
            ScanCandidateKind::PkInterval,
            String::new(),
            Some(ScanBound::PkSet(bound)),
            filter,
        ));
    }
    for idx in &rel.table.indexes {
        if let Some(bound) = build_index_interval_set_plan(filter_expr, rel, idx, catalog) {
            let name = bound.name_key.clone();
            candidates.push(index_order_candidate(
                ScanCandidateKind::IndexInterval,
                name,
                ScanBound::IndexSet(bound),
                filter,
            ));
        }
    }
    candidates.push(full());
    candidates.sort_by(|a, b| {
        a.identity.kind.cmp(&b.identity.kind).then_with(|| {
            a.identity
                .index_name
                .as_bytes()
                .cmp(b.identity.index_name.as_bytes())
        })
    });
    candidates
}

fn estimator_predicate_selectivity(expr: Option<&RExpr>) -> crate::estimator::Selectivity {
    use crate::estimator::Selectivity;
    use crate::estimator_constants::*;
    let Some(expr) = expr else {
        return Selectivity::All;
    };
    match expr {
        RExpr::ConstBool(true) => Selectivity::All,
        RExpr::ConstBool(false) | RExpr::ConstNull => Selectivity::Zero,
        RExpr::And(..) => {
            let mut conjuncts = Vec::new();
            estimator_flatten_boolean(expr, true, &mut conjuncts);
            if estimator_conjunction_contradictory(&conjuncts) {
                return Selectivity::Zero;
            }
            let mut used = vec![false; conjuncts.len()];
            let mut result = Selectivity::All;
            for i in 0..conjuncts.len() {
                if used[i] {
                    continue;
                }
                let paired = ((i + 1)..conjuncts.len())
                    .find(|j| !used[*j] && estimator_paired_range(conjuncts[i], conjuncts[*j]));
                if let Some(j) = paired {
                    used[j] = true;
                    result = result.and(Selectivity::fraction(SELECTIVITY_PAIRED_RANGE));
                } else {
                    result = result.and(estimator_predicate_selectivity(Some(conjuncts[i])));
                }
            }
            result
        }
        RExpr::Or(..) => {
            let mut disjuncts = Vec::new();
            estimator_flatten_boolean(expr, false, &mut disjuncts);
            let mut result: Option<Selectivity> = None;
            for (i, disjunct) in disjuncts.iter().enumerate() {
                let duplicate =
                    estimator_equality_parts(disjunct).is_some_and(|(operand, literal)| {
                        disjuncts[..i].iter().any(|prior| {
                            estimator_equality_parts(prior).is_some_and(
                                |(prior_operand, prior_literal)| {
                                    rexpr_eq_shifted(operand, prior_operand, 0)
                                        && rexpr_eq_shifted(literal, prior_literal, 0)
                                },
                            )
                        })
                    });
                if duplicate {
                    continue;
                }
                let part = estimator_predicate_selectivity(Some(disjunct));
                result = Some(match result {
                    None => part,
                    Some(lhs) => lhs.or(part),
                });
            }
            result.unwrap_or(Selectivity::Zero)
        }
        RExpr::Not(child) => estimator_predicate_selectivity(Some(child)).not(),
        RExpr::Compare { lhs, rhs, .. }
            if matches!(lhs.as_ref(), RExpr::ConstNull)
                || matches!(rhs.as_ref(), RExpr::ConstNull) =>
        {
            Selectivity::Zero
        }
        RExpr::Compare { op: CmpOp::Eq, .. } => Selectivity::fraction(SELECTIVITY_EQUALITY),
        RExpr::Compare { op: CmpOp::Ne, .. } => Selectivity::fraction(SELECTIVITY_EQUALITY).not(),
        RExpr::Compare { .. } => Selectivity::fraction(SELECTIVITY_INEQUALITY),
        RExpr::Distinct { negated, .. } => {
            let equality = Selectivity::fraction(SELECTIVITY_EQUALITY);
            if *negated { equality } else { equality.not() }
        }
        RExpr::IsNull { negated, .. } => {
            let null_test = Selectivity::fraction(SELECTIVITY_NULL_TEST);
            if *negated { null_test.not() } else { null_test }
        }
        RExpr::Like { negated, .. } | RExpr::Regex { negated, .. } => {
            let matched = Selectivity::fraction(SELECTIVITY_MATCH);
            if *negated { matched.not() } else { matched }
        }
        RExpr::Column(_) => Selectivity::fraction(SELECTIVITY_BOOLEAN),
        _ => crate::estimator::selectivity_class(ACCESS_UNSUPPORTED),
    }
}

fn estimator_flatten_boolean<'a>(expr: &'a RExpr, and: bool, out: &mut Vec<&'a RExpr>) {
    match (and, expr) {
        (true, RExpr::And(lhs, rhs)) | (false, RExpr::Or(lhs, rhs)) => {
            estimator_flatten_boolean(lhs, and, out);
            estimator_flatten_boolean(rhs, and, out);
        }
        _ => out.push(expr),
    }
}

fn estimator_literal(expr: &RExpr) -> bool {
    matches!(
        expr,
        RExpr::ConstInt(_)
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
            | RExpr::ConstNull
    )
}

fn estimator_equality_parts(expr: &RExpr) -> Option<(&RExpr, &RExpr)> {
    let RExpr::Compare {
        op: CmpOp::Eq,
        lhs,
        rhs,
        ..
    } = expr
    else {
        return None;
    };
    if estimator_literal(rhs) && !rexpr_is_constant(lhs) {
        return Some((lhs, rhs));
    }
    if estimator_literal(lhs) && !rexpr_is_constant(rhs) {
        return Some((rhs, lhs));
    }
    None
}

struct EstimatorComparison<'a> {
    operand: &'a RExpr,
    literal: &'a RExpr,
    op: CmpOp,
}

fn estimator_comparison_parts(expr: &RExpr) -> Option<EstimatorComparison<'_>> {
    let RExpr::Compare { op, lhs, rhs, .. } = expr else {
        return None;
    };
    if !matches!(
        op,
        CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge
    ) {
        return None;
    }
    if estimator_literal(rhs) && !rexpr_is_constant(lhs) {
        return Some(EstimatorComparison {
            operand: lhs,
            literal: rhs,
            op: *op,
        });
    }
    if estimator_literal(lhs) && !rexpr_is_constant(rhs) {
        return Some(EstimatorComparison {
            operand: rhs,
            literal: lhs,
            op: flip_cmp(*op),
        });
    }
    None
}

// Compare resolved, same-kind plan-time literals by their SQL total order. Open/unsupported
// literal kinds return None: missing a proof is safe, inventing one is not.
fn estimator_literal_cmp(a: &RExpr, b: &RExpr) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (RExpr::ConstInt(x), RExpr::ConstInt(y)) => Some(x.cmp(y)),
        (RExpr::ConstBool(x), RExpr::ConstBool(y)) => Some(x.cmp(y)),
        (RExpr::ConstText(x), RExpr::ConstText(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (RExpr::ConstDecimal(x), RExpr::ConstDecimal(y)) => Some(x.cmp_value(y)),
        (RExpr::ConstFloat32(x), RExpr::ConstFloat32(y)) => {
            Some(crate::value::total_cmp_f32(*x, *y))
        }
        (RExpr::ConstFloat64(x), RExpr::ConstFloat64(y)) => {
            Some(crate::value::total_cmp_f64(*x, *y))
        }
        (RExpr::ConstBytea(x), RExpr::ConstBytea(y)) => Some(x.cmp(y)),
        (RExpr::ConstUuid(x), RExpr::ConstUuid(y)) => Some(x.cmp(y)),
        (RExpr::ConstTimestamp(x), RExpr::ConstTimestamp(y))
        | (RExpr::ConstTimestamptz(x), RExpr::ConstTimestamptz(y)) => Some(x.cmp(y)),
        (RExpr::ConstDate(x), RExpr::ConstDate(y)) => Some(x.cmp(y)),
        (RExpr::ConstInterval(x), RExpr::ConstInterval(y)) => Some(x.span().cmp(&y.span())),
        _ => None,
    }
}

fn estimator_comparison_satisfied(order: std::cmp::Ordering, op: CmpOp) -> bool {
    use std::cmp::Ordering;
    match op {
        CmpOp::Eq => order == Ordering::Equal,
        CmpOp::Lt => order == Ordering::Less,
        CmpOp::Le => order != Ordering::Greater,
        CmpOp::Gt => order == Ordering::Greater,
        CmpOp::Ge => order != Ordering::Less,
        CmpOp::Ne => true,
    }
}

fn estimator_comparisons_contradict(
    a: &EstimatorComparison<'_>,
    b: &EstimatorComparison<'_>,
) -> bool {
    use std::cmp::Ordering;
    if !rexpr_eq_shifted(a.operand, b.operand, 0) {
        return false;
    }
    // Text equality is byte identity, but range order may use a derived collation unavailable to
    // this structural fold. Decline that proof rather than applying raw UTF-8 order.
    if (matches!(a.literal, RExpr::ConstText(_)) || matches!(b.literal, RExpr::ConstText(_)))
        && (a.op != CmpOp::Eq || b.op != CmpOp::Eq)
    {
        return false;
    }
    if a.op == CmpOp::Eq {
        return estimator_literal_cmp(a.literal, b.literal)
            .is_some_and(|order| !estimator_comparison_satisfied(order, b.op));
    }
    if b.op == CmpOp::Eq {
        return estimator_literal_cmp(b.literal, a.literal)
            .is_some_and(|order| !estimator_comparison_satisfied(order, a.op));
    }
    let a_lower = matches!(a.op, CmpOp::Gt | CmpOp::Ge);
    let b_lower = matches!(b.op, CmpOp::Gt | CmpOp::Ge);
    if a_lower == b_lower {
        return false;
    }
    let (lower, upper) = if a_lower { (a, b) } else { (b, a) };
    estimator_literal_cmp(lower.literal, upper.literal).is_some_and(|order| {
        order == Ordering::Greater
            || (order == Ordering::Equal && (lower.op == CmpOp::Gt || upper.op == CmpOp::Lt))
    })
}

fn estimator_conjunction_contradictory(conjuncts: &[&RExpr]) -> bool {
    let mut comparisons = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts {
        let Some(comparison) = estimator_comparison_parts(conjunct) else {
            continue;
        };
        if matches!(comparison.literal, RExpr::ConstNull)
            || comparisons
                .iter()
                .any(|prior| estimator_comparisons_contradict(prior, &comparison))
        {
            return true;
        }
        comparisons.push(comparison);
    }
    false
}

fn estimator_range_operand(expr: &RExpr) -> Option<(&RExpr, bool)> {
    let comparison = estimator_comparison_parts(expr)?;
    (comparison.op != CmpOp::Eq).then_some((
        comparison.operand,
        matches!(comparison.op, CmpOp::Gt | CmpOp::Ge),
    ))
}

fn estimator_paired_range(lhs: &RExpr, rhs: &RExpr) -> bool {
    let (Some((a, a_lower)), Some((b, b_lower))) =
        (estimator_range_operand(lhs), estimator_range_operand(rhs))
    else {
        return false;
    };
    a_lower != b_lower && rexpr_eq_shifted(a, b, 0)
}

fn estimator_bound_src_cmp(a: &BoundSrc, b: &BoundSrc) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (BoundSrc::Int(x), BoundSrc::Int(y)) => Some(x.cmp(y)),
        (BoundSrc::Bool(x), BoundSrc::Bool(y)) => Some(x.cmp(y)),
        (BoundSrc::Uuid(x), BoundSrc::Uuid(y)) => Some(x.cmp(y)),
        (BoundSrc::Timestamp(x), BoundSrc::Timestamp(y)) => Some(x.cmp(y)),
        (BoundSrc::Date(x), BoundSrc::Date(y)) => Some(x.cmp(y)),
        (BoundSrc::Text(x), BoundSrc::Text(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (BoundSrc::Bytea(x), BoundSrc::Bytea(y)) => Some(x.cmp(y)),
        (BoundSrc::Decimal(x), BoundSrc::Decimal(y)) => Some(x.cmp_value(y)),
        (BoundSrc::Interval(x), BoundSrc::Interval(y)) => Some(x.span().cmp(&y.span())),
        _ => None,
    }
}

fn estimator_bound_terms_contradict(a: &BoundTerm, b: &BoundTerm) -> bool {
    use std::cmp::Ordering;
    // Text range order may be collated; equality remains byte identity.
    if (matches!(a.src, BoundSrc::Text(_)) || matches!(b.src, BoundSrc::Text(_)))
        && (a.op != CmpOp::Eq || b.op != CmpOp::Eq)
    {
        return false;
    }
    if a.op == CmpOp::Eq {
        return estimator_bound_src_cmp(&a.src, &b.src)
            .is_some_and(|order| !estimator_comparison_satisfied(order, b.op));
    }
    if b.op == CmpOp::Eq {
        return estimator_bound_src_cmp(&b.src, &a.src)
            .is_some_and(|order| !estimator_comparison_satisfied(order, a.op));
    }
    let a_lower = matches!(a.op, CmpOp::Gt | CmpOp::Ge);
    let b_lower = matches!(b.op, CmpOp::Gt | CmpOp::Ge);
    if a_lower == b_lower {
        return false;
    }
    let (lower, upper) = if a_lower { (a, b) } else { (b, a) };
    estimator_bound_src_cmp(&lower.src, &upper.src).is_some_and(|order| {
        order == Ordering::Greater
            || (order == Ordering::Equal && (lower.op == CmpOp::Gt || upper.op == CmpOp::Lt))
    })
}

fn estimator_equality_sources_impossible(sources: &[BoundSrc], key_type: ScalarType) -> bool {
    sources.iter().enumerate().any(|(i, source)| {
        matches!(source, BoundSrc::Null)
            || matches!(source, BoundSrc::Int(value) if key_type.is_integer() && !key_type.in_range(*value))
            || sources[..i].iter().any(|prior| {
                estimator_bound_src_cmp(prior, source)
                    .is_some_and(|order| order != std::cmp::Ordering::Equal)
            })
    })
}

fn estimator_range_terms(
    terms: &[BoundTerm],
    key_type: ScalarType,
) -> crate::estimator::Selectivity {
    use crate::estimator::Selectivity;
    use crate::estimator_constants::*;
    if terms.is_empty() {
        return Selectivity::All;
    }
    if terms.iter().any(|term| matches!(&term.src, BoundSrc::Null)) {
        return Selectivity::Zero;
    }
    if terms.iter().enumerate().any(|(i, term)| {
        matches!(&term.src, BoundSrc::Int(value) if term.op == CmpOp::Eq && key_type.is_integer() && !key_type.in_range(*value))
            || terms[..i]
                .iter()
                .any(|prior| estimator_bound_terms_contradict(prior, term))
    }) {
        return Selectivity::Zero;
    }
    if terms.iter().any(|term| term.op == CmpOp::Eq) {
        return Selectivity::fraction(SELECTIVITY_EQUALITY);
    }
    let lower = terms
        .iter()
        .any(|term| matches!(term.op, CmpOp::Gt | CmpOp::Ge));
    let upper = terms
        .iter()
        .any(|term| matches!(term.op, CmpOp::Lt | CmpOp::Le));
    Selectivity::fraction(if lower && upper {
        SELECTIVITY_PAIRED_RANGE
    } else {
        SELECTIVITY_INEQUALITY
    })
}

fn estimator_equality_prefix(count: usize) -> crate::estimator::Selectivity {
    use crate::estimator::Selectivity;
    use crate::estimator_constants::SELECTIVITY_EQUALITY;
    let mut result = Selectivity::All;
    for _ in 0..count {
        result = result.and(Selectivity::fraction(SELECTIVITY_EQUALITY));
    }
    result
}

fn estimator_interval_selectivity(
    specs: &[IntervalSpec],
    clip: &[BoundTerm],
    unique_points: bool,
    key_type: ScalarType,
) -> crate::estimator::Selectivity {
    use crate::estimator::Selectivity;
    let mut disjunction: Option<Selectivity> = None;
    for spec in specs {
        let structural = estimator_range_terms(&spec.terms, key_type);
        let term = if matches!(structural, Selectivity::Zero) {
            structural
        } else if unique_points
            && !spec.terms.is_empty()
            && spec.terms.iter().all(|term| term.op == CmpOp::Eq)
        {
            Selectivity::Unique
        } else {
            structural
        };
        disjunction = Some(match disjunction {
            None => term,
            Some(lhs) => lhs.or(term),
        });
    }
    let mut result = disjunction.unwrap_or(Selectivity::Zero);
    if !clip.is_empty() {
        result = result.and(estimator_range_terms(clip, key_type));
    }
    result
}

fn estimator_candidate_selectivity(
    candidate: &ScanCandidate<'_>,
    rel: &ScopeRel<'_>,
) -> crate::estimator::Selectivity {
    use crate::estimator::Selectivity;
    use crate::estimator::selectivity_class;
    use crate::estimator_constants::*;
    match candidate.bound.as_ref() {
        Some(ScanBound::Pk(bound)) => {
            if bound
                .eq_cols
                .iter()
                .any(|column| estimator_equality_sources_impossible(&column.srcs, column.col_type))
            {
                return Selectivity::Zero;
            }
            if bound.eq_cols.len() == bound.member_count && bound.range.is_none() {
                return Selectivity::Unique;
            }
            let mut result = estimator_equality_prefix(bound.eq_cols.len());
            if let Some(range) = &bound.range {
                result = result.and(estimator_range_terms(&range.terms, range.col_type));
            }
            result
        }
        Some(ScanBound::Index(bound)) => {
            if bound
                .eq_cols
                .iter()
                .any(|column| estimator_equality_sources_impossible(&column.srcs, column.col_type))
            {
                return Selectivity::Zero;
            }
            let unique = rel.table.indexes.iter().any(|index| {
                index
                    .name
                    .eq_ignore_ascii_case(&candidate.identity.index_name)
                    && index.unique
                    && bound.eq_cols.len() == index.keys.len()
                    && bound.range.is_none()
            });
            if unique {
                return Selectivity::Unique;
            }
            let mut result = estimator_equality_prefix(bound.eq_cols.len());
            if let Some(range) = &bound.range {
                result = result.and(estimator_range_terms(&range.terms, range.col_type));
            }
            result
        }
        Some(ScanBound::Gist(bound)) => {
            if bound.strategy == crate::gist::GistStrategy::Equal {
                selectivity_class(ACCESS_GIST_EQUAL)
            } else {
                selectivity_class(ACCESS_GIST_RANGE)
            }
        }
        Some(ScanBound::Gin(bound)) => selectivity_class(match bound.strategy {
            GinStrategy::Contains => ACCESS_GIN_CONTAINS,
            GinStrategy::Overlaps => ACCESS_GIN_OVERLAPS,
            GinStrategy::Member => ACCESS_GIN_MEMBER,
            GinStrategy::Equal => ACCESS_GIN_EQUAL,
        }),
        Some(ScanBound::PkSet(bound)) => {
            estimator_interval_selectivity(&bound.specs, &bound.clip, true, bound.pk_type)
        }
        Some(ScanBound::IndexSet(bound)) => {
            let unique = rel.table.indexes.iter().any(|index| {
                index
                    .name
                    .eq_ignore_ascii_case(&candidate.identity.index_name)
                    && index.unique
                    && index.keys.len() == 1
            });
            estimator_interval_selectivity(&bound.specs, &bound.clip, unique, bound.col_type)
        }
        None => Selectivity::All,
    }
}

fn estimator_operator_nodes(expr: Option<&RExpr>) -> i64 {
    use crate::estimator::sat_add;
    let Some(expr) = expr else { return 0 };
    let children: i64 = match expr {
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
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::DateClock { .. }
        | RExpr::ConstNull => return 0,
        RExpr::Subquery { lhs, .. } => estimator_operator_nodes(lhs.as_deref()),
        RExpr::InValues { lhs, .. } => estimator_operator_nodes(Some(lhs)),
        RExpr::Quantified { lhs, array, .. }
        | RExpr::Arith {
            lhs, rhs: array, ..
        }
        | RExpr::Compare {
            lhs, rhs: array, ..
        }
        | RExpr::Distinct {
            lhs, rhs: array, ..
        }
        | RExpr::Like {
            lhs, rhs: array, ..
        }
        | RExpr::Regex {
            lhs, rhs: array, ..
        } => sat_add(
            estimator_operator_nodes(Some(lhs)),
            estimator_operator_nodes(Some(array)),
        ),
        RExpr::And(lhs, rhs) | RExpr::Or(lhs, rhs) => sat_add(
            estimator_operator_nodes(Some(lhs)),
            estimator_operator_nodes(Some(rhs)),
        ),
        RExpr::Cast { inner, .. }
        | RExpr::ArrayCast { inner, .. }
        | RExpr::DateConvert { inner, .. } => estimator_operator_nodes(Some(inner)),
        RExpr::Neg { operand, .. }
        | RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => estimator_operator_nodes(Some(operand)),
        RExpr::Not(child) => estimator_operator_nodes(Some(child)),
        RExpr::Casing { arg, .. } => estimator_operator_nodes(Some(arg)),
        RExpr::AtTimeZone { zone, value, .. } => sat_add(
            estimator_operator_nodes(Some(zone)),
            estimator_operator_nodes(Some(value)),
        ),
        RExpr::DateTrunc { unit, value, zone } => sat_add(
            sat_add(
                estimator_operator_nodes(Some(unit)),
                estimator_operator_nodes(Some(value)),
            ),
            estimator_operator_nodes(zone.as_deref()),
        ),
        RExpr::Extract { value, .. } => estimator_operator_nodes(Some(value)),
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => sat_add(
            estimator_operator_nodes(Some(base)),
            estimator_operator_nodes(Some(arg)),
        ),
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => sat_add(
            estimator_operator_nodes(Some(a)),
            estimator_operator_nodes(Some(b)),
        ),
        RExpr::Case { arms, els, .. } => arms.iter().fold(
            estimator_operator_nodes(Some(els)),
            |total, (condition, result)| {
                sat_add(
                    total,
                    sat_add(
                        estimator_operator_nodes(Some(condition)),
                        estimator_operator_nodes(Some(result)),
                    ),
                )
            },
        ),
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
        | RExpr::JsonPathFn { args, .. } => args.iter().fold(0, |total, arg| {
            sat_add(total, estimator_operator_nodes(Some(arg)))
        }),
        RExpr::JsonSqlFn { ctx, path, .. } => sat_add(
            estimator_operator_nodes(Some(ctx)),
            estimator_operator_nodes(Some(path)),
        ),
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            fields.iter().fold(0, |total, field| {
                sat_add(total, estimator_operator_nodes(Some(field)))
            })
        }
        RExpr::Field { base, .. } => estimator_operator_nodes(Some(base)),
        RExpr::Subscript {
            base, subscripts, ..
        } => subscripts
            .iter()
            .flat_map(subscript_bounds)
            .fold(estimator_operator_nodes(Some(base)), |total, bound| {
                sat_add(total, estimator_operator_nodes(Some(bound)))
            }),
    };
    sat_add(1, children)
}

fn estimator_clamp_pages(rows: i64, nodes: i64, height: i64) -> i64 {
    if rows == 0 || nodes == 0 {
        0
    } else {
        rows.max(height).min(nodes)
    }
}

pub(crate) fn estimate_scan_candidates(
    candidates: &[ScanCandidate<'_>],
    rel: &ScopeRel<'_>,
    catalog: &Engine,
    produces_rows: bool,
) -> Vec<crate::estimator::CandidateEstimate> {
    use crate::estimator::{CandidateInputs, estimate_candidate, estimate_rows};
    let store = catalog.store_scoped(rel.db.as_deref(), &rel.table.name);
    let row_count = store.count().unwrap_or(0);
    let selectivities: Vec<_> = candidates
        .iter()
        .map(|candidate| estimator_candidate_selectivity(candidate, rel))
        .collect();
    let output_selectivity = if selectivities
        .iter()
        .any(|selectivity| matches!(selectivity, crate::estimator::Selectivity::Zero))
    {
        crate::estimator::Selectivity::Zero
    } else {
        estimator_predicate_selectivity(candidates.first().and_then(|c| c.residual))
    };
    let output_rows = estimate_rows(&output_selectivity, row_count);
    let table_height = store.height() as i64;
    let filter_nodes = estimator_operator_nodes(candidates.first().and_then(|c| c.residual));
    candidates
        .iter()
        .zip(selectivities)
        .map(|(candidate, selectivity)| {
            let kind =
                crate::estimator_constants::ACCESS_PATH_ORDER[candidate.identity.kind as usize];
            let scan_rows = estimate_rows(&selectivity, row_count);
            let (access_nodes, access_height) = match candidate.identity.kind {
                ScanCandidateKind::Btree
                | ScanCandidateKind::Gin
                | ScanCandidateKind::IndexInterval => {
                    let index = catalog
                        .index_store_scoped(rel.db.as_deref(), &candidate.identity.index_name);
                    (index.node_count() as i64, index.height() as i64)
                }
                _ => (store.node_count() as i64, table_height),
            };
            let access_pages = if candidate.identity.kind == ScanCandidateKind::Full {
                access_nodes
            } else {
                estimator_clamp_pages(
                    estimate_rows(&selectivity, access_nodes),
                    access_nodes,
                    access_height,
                )
            };
            let access_work = match candidate.identity.kind {
                ScanCandidateKind::Gin => scan_rows,
                ScanCandidateKind::Gist => access_pages,
                _ => 0,
            };
            estimate_candidate(CandidateInputs {
                kind,
                index_name: &candidate.identity.index_name,
                scan_rows,
                output_rows,
                access_pages,
                table_height,
                filter_nodes,
                access_work,
                produces_rows,
            })
        })
        .collect()
}

fn candidate_index(
    candidates: &[ScanCandidate<'_>],
    kind: ScanCandidateKind,
    name: Option<&str>,
) -> Option<usize> {
    candidates.iter().position(|candidate| {
        candidate.identity.kind == kind
            && name.is_none_or(|name| candidate.identity.index_name == name)
    })
}

fn take_candidate(
    candidates: &mut [ScanCandidate<'_>],
    kind: ScanCandidateKind,
    name: Option<&str>,
) -> Option<ScanBound> {
    let i = candidate_index(candidates, kind, name)?;
    candidates[i].bound.take()
}

/// Reproduce the pre-P3 policy exactly. This order deliberately differs from the canonical cost-tie
/// order for clipped same-key interval sets and for mutation's GIN-before-GiST precedence.
pub(crate) fn select_legacy_scan_candidate(
    mut candidates: Vec<ScanCandidate<'_>>,
    policy: ScanBoundPolicy,
) -> Option<ScanBound> {
    if policy.index_set {
        if let Some(i) = candidate_index(&candidates, ScanCandidateKind::PkInterval, None) {
            if matches!(&candidates[i].bound, Some(ScanBound::PkSet(set)) if !set.clip.is_empty()) {
                return candidates[i].bound.take();
            }
        }
    }
    if let Some(bound) = take_candidate(&mut candidates, ScanCandidateKind::Pk, None) {
        return Some(bound);
    }
    if policy.ordered_index {
        let btree_names: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.identity.kind == ScanCandidateKind::Btree)
            .map(|candidate| candidate.identity.index_name.clone())
            .collect();
        for name in btree_names {
            if policy.index_set {
                if let Some(i) =
                    candidate_index(&candidates, ScanCandidateKind::IndexInterval, Some(&name))
                {
                    if matches!(&candidates[i].bound, Some(ScanBound::IndexSet(set)) if !set.clip.is_empty())
                    {
                        return candidates[i].bound.take();
                    }
                }
            }
            if let Some(bound) =
                take_candidate(&mut candidates, ScanCandidateKind::Btree, Some(&name))
            {
                return Some(bound);
            }
        }
    }
    let (first, second) = if policy.gist_before_gin {
        (ScanCandidateKind::Gist, ScanCandidateKind::Gin)
    } else {
        (ScanCandidateKind::Gin, ScanCandidateKind::Gist)
    };
    if let Some(bound) = take_candidate(&mut candidates, first, None) {
        return Some(bound);
    }
    if let Some(bound) = take_candidate(&mut candidates, second, None) {
        return Some(bound);
    }
    if policy.index_set {
        if let Some(bound) = take_candidate(&mut candidates, ScanCandidateKind::PkInterval, None) {
            return Some(bound);
        }
        if let Some(bound) = take_candidate(&mut candidates, ScanCandidateKind::IndexInterval, None)
        {
            return Some(bound);
        }
    }
    None
}

/// Compatibility entry point used by SELECT and UPDATE/DELETE. P6 replaces only this selector for
/// eligible SELECT relations; candidate inventory remains unchanged.
pub(crate) fn detect_scan_bound_with_policy(
    filter: &RExpr,
    rel: &ScopeRel,
    catalog: &Engine,
    policy: ScanBoundPolicy,
) -> Option<ScanBound> {
    select_legacy_scan_candidate(
        inventory_scan_candidates(Some(filter), rel, catalog),
        policy,
    )
}

fn interval_plan_has_range(specs: &[IntervalSpec], clip: &[BoundTerm]) -> bool {
    specs
        .iter()
        .flat_map(|s| &s.terms)
        .chain(clip)
        .any(|t| !matches!(t.op, CmpOp::Eq))
}

fn build_index_interval_set_plan(
    filter: &RExpr,
    rel: &ScopeRel,
    idx: &IndexDef,
    catalog: &Engine,
) -> Option<IndexKeySet> {
    if idx.kind != IndexKind::Btree || idx.predicate.is_some() {
        return None;
    }
    let cols = idx.column_ordinals()?;
    let ci = cols[0];
    let ty = rel.table.columns[ci].ty.as_scalar()?;
    if cols[1..].iter().any(|&c| {
        rel.table.columns[c]
            .ty
            .as_scalar()
            .is_none_or(|s| !s.is_fixed_width())
    }) {
        return None;
    }
    let coll = key_collation_ctx(catalog, &rel.table.columns[ci])?;
    let (specs, clip) = detect_interval_set(filter, rel.offset + ci, ty, coll.as_deref())?;
    if interval_plan_has_range(&specs, &clip) && !ty.is_fixed_width() {
        return None;
    }
    Some(IndexKeySet {
        name_key: idx.name.to_ascii_lowercase(),
        col_type: ty,
        coll,
        tail_types: cols[1..]
            .iter()
            .map(|&c| rel.table.columns[c].ty.scalar())
            .collect(),
        specs,
        clip,
    })
}

impl Engine {
    /// Select an UPDATE/DELETE target access path through the same inventory as SELECT, using the
    /// mutation eligibility policy. Execution calls this after uncorrelated filter folding, matching
    /// the old inline detector timing; EXPLAIN calls it on its resolved, unfolded filter.
    pub(crate) fn plan_mutation_scan(
        &self,
        db: Option<&str>,
        table: &Table,
        filter: Option<&RExpr>,
    ) -> MutationScanPlan {
        let bound = filter.and_then(|f| {
            let rel = ScopeRel {
                label: table.name.to_ascii_lowercase(),
                table,
                offset: 0,
                qualifier_only: false,
                cte: None,
                db: db.map(str::to_owned),
            };
            detect_scan_bound_with_policy(f, &rel, self, MUTATION_SCAN_BOUND_POLICY)
        });
        MutationScanPlan {
            bound,
            db: db.map(str::to_owned),
        }
    }
}

/// Find the first top-level conjunct that is a pure OR of intervals on one key. A leaf is a single
/// comparison or an AND of same-key comparisons (BETWEEN's resolved shape). Direct same-key
/// comparisons in the remaining top-level conjuncts become a global clip. The full predicate stays
/// residual, so rejecting any other shape is conservative and widening an unencodable endpoint is
/// sound.
pub(crate) fn detect_interval_set(
    filter: &RExpr,
    key_idx: usize,
    key_type: ScalarType,
    coll: Option<&Collation>,
) -> Option<(Vec<IntervalSpec>, Vec<BoundTerm>)> {
    let col_coll = coll.map(|c| c.name.as_str());
    fn flatten<'a>(e: &'a RExpr, out: &mut Vec<&'a RExpr>) {
        if let RExpr::And(l, r) = e {
            flatten(l, out);
            flatten(r, out);
        } else {
            out.push(e);
        }
    }
    let mut conjuncts = Vec::new();
    flatten(filter, &mut conjuncts);
    let mut found = None;
    for (i, e) in conjuncts.iter().enumerate() {
        if !matches!(e, RExpr::Or(_, _)) {
            continue;
        }
        if let Some(specs) = reduce_interval_union(e, key_idx, key_type, col_coll) {
            found = Some((i, specs));
            break;
        }
    }
    let (found_idx, specs) = found?;
    let mut clip = Vec::new();
    for (i, e) in conjuncts.into_iter().enumerate() {
        if i != found_idx {
            collect_bound_terms(
                e,
                &KeyMatch::Column(key_idx),
                key_type,
                col_coll,
                None,
                &mut clip,
            );
        }
    }
    Some((specs, clip))
}

/// Reduce one pure OR tree to interval specs. Each non-OR leaf may be a conjunction, but every term
/// must bound this key with a matching type and collation.
pub(crate) fn reduce_interval_union(
    e: &RExpr,
    key_idx: usize,
    key_type: ScalarType,
    col_coll: Option<&str>,
) -> Option<Vec<IntervalSpec>> {
    if let RExpr::Or(l, r) = e {
        let mut left = reduce_interval_union(l, key_idx, key_type, col_coll)?;
        let right = reduce_interval_union(r, key_idx, key_type, col_coll)?;
        left.extend(right);
        return Some(left);
    }
    fn leaf_terms(
        e: &RExpr,
        key_idx: usize,
        key_type: ScalarType,
        col_coll: Option<&str>,
        out: &mut Vec<BoundTerm>,
    ) -> bool {
        if let RExpr::And(l, r) = e {
            return leaf_terms(l, key_idx, key_type, col_coll, out)
                && leaf_terms(r, key_idx, key_type, col_coll, out);
        }
        let before = out.len();
        collect_bound_terms(e, &KeyMatch::Column(key_idx), key_type, col_coll, None, out);
        out.len() == before + 1
    }
    let mut terms = Vec::new();
    leaf_terms(e, key_idx, key_type, col_coll, &mut terms)
        .then_some(IntervalSpec { terms })
        .map(|s| vec![s])
}

/// Detect an **index-nested-loop** scan bound for a join inner relation `rel` (spec/design/cost.md
/// §3 "JOIN"): a primary-key (or leading secondary-index column) comparison to a **sibling** column
/// of an EARLIER join relation, taken from the join's `on` predicate OR the `where` filter. Unlike
/// [`detect_scan_bound`] (constants only), this admits a bare sibling column (`BoundSrc::Sibling`,
/// enabled by `sibling_cutoff = rel.offset`), resolved per outer row from the current combined
/// left-hand row — the join analog of a correlated subquery's outer reference
/// (`query.correlated_pushdown`). So the inner relation seeks per outer row instead of full-scanning
/// for every outer row: O(N·M) → O(N·log M).
///
/// Returns `Some` only when the resulting bound has **≥ 1 sibling term** — a constant-only bound is
/// the ordinary once-materialized `rel_bounds` path, not index-nested-loop. Constant terms on the
/// same key that co-occur (`b.pk = a.x AND b.pk = 5`) ride along and tighten the per-outer-row seek.
/// The whole `on`/`where` stays the residual filter (the bound is a superset of the matching rows),
/// so the **rows are unchanged**; only the inner re-scan cost drops. Caller restricts this to a base
/// table that is the right/nullable side of an INNER/CROSS/LEFT join (a RIGHT/FULL preserved side
/// cannot be bounded per outer row — it would drop rows matching no outer row).
pub(crate) fn detect_inl_bound(
    on: Option<&RExpr>,
    where_filter: Option<&RExpr>,
    rel: &ScopeRel,
    catalog: &Engine,
) -> Option<ScanBound> {
    // A host-attached inner relation full-scans per outer row this slice (attached-databases.md §8):
    // the seek would resolve its index store unscoped. Index-nested-loop over an attachment is a
    // perf follow-on.
    if rel.is_attachment() {
        return None;
    }
    let cutoff = Some(rel.offset);
    // Collect the key's bound terms from BOTH the ON and the WHERE (a NULL predicate contributes
    // none), with sibling columns admitted.
    let collect = |key_idx: usize, ty: ScalarType, ccoll: Option<&str>| -> Vec<BoundTerm> {
        let mut terms = Vec::new();
        let km = KeyMatch::Column(key_idx);
        if let Some(f) = on {
            collect_bound_terms(f, &km, ty, ccoll, cutoff, &mut terms);
        }
        if let Some(f) = where_filter {
            collect_bound_terms(f, &km, ty, ccoll, cutoff, &mut terms);
        }
        terms
    };
    // Primary-key tuple bound first (the row's own key — range-capable, strictly cheaper).
    let filters: Vec<&RExpr> = on.into_iter().chain(where_filter).collect();
    if let Some(b) = detect_pk_bound(&filters, rel, cutoff, catalog) {
        let has_sibling = b.eq_cols.iter().any(|ec| {
            ec.srcs.iter().any(|s| matches!(s, BoundSrc::Sibling(_)))
                || ec
                    .ranges
                    .iter()
                    .any(|t| matches!(t.src, BoundSrc::Sibling(_)))
        }) || b.range.as_ref().is_some_and(|r| {
            r.terms
                .iter()
                .any(|t| matches!(t.src, BoundSrc::Sibling(_)))
        });
        if has_sibling {
            return Some(ScanBound::Pk(b));
        }
    }
    // Else a leading secondary-index equality bound to a sibling (indexes held in ascending
    // lowercased-name order — the deterministic tie-break, matching detect_scan_bound).
    for idx in &rel.table.indexes {
        if idx.kind != IndexKind::Btree {
            continue;
        }
        // The index-nested-loop sibling bound is column-only this slice (an expression index takes
        // the access-predicate path — indexes.md §5; an INL bound over an expression key is a follow-on).
        let Some(cols) = idx.column_ordinals() else {
            continue;
        };
        let ci = cols[0];
        let Some(ty) = rel.table.columns[ci].ty.as_scalar() else {
            continue;
        };
        if cols[1..].iter().any(|&c| {
            rel.table.columns[c]
                .ty
                .as_scalar()
                .is_none_or(|s| !s.is_fixed_width())
        }) {
            continue;
        }
        let Some(coll) = key_collation_ctx(catalog, &rel.table.columns[ci]) else {
            continue;
        };
        let terms = collect(rel.offset + ci, ty, coll.as_ref().map(|c| c.name.as_str()));
        let eqs: Vec<BoundSrc> = terms
            .into_iter()
            .filter(|t| matches!(t.op, CmpOp::Eq))
            .map(|t| t.src)
            .collect();
        if eqs.iter().any(|s| matches!(s, BoundSrc::Sibling(_))) {
            // This slice keeps the index-nested-loop bound single-column-equality (a leading key
            // column bound to a sibling); a multi-column / range INL bound is a follow-on (cost.md
            // §3 "index-nested-loop"). `suffix_types` are the trailing columns (columns[1..],
            // fixed-width by the check above), width-skipped past the single equality slot.
            return Some(ScanBound::Index(IndexBound {
                name_key: idx.name.to_ascii_lowercase(),
                eq_cols: vec![IndexEqCol {
                    col_type: ty,
                    coll,
                    srcs: eqs,
                }],
                range: None,
                suffix_types: cols[1..]
                    .iter()
                    .map(|&c| rel.table.columns[c].ty.scalar())
                    .collect(),
            }));
        }
    }
    // Opclass sibling bounds follow the cheaper primary-key and ordered-B-tree paths. GiST precedes
    // GIN, matching ordinary SELECT planning. Only a bare earlier-sibling column is admissible.
    for idx in &rel.table.indexes {
        if idx.kind != IndexKind::Gist || idx.keys.len() != 1 {
            continue;
        }
        let ci = idx.first_column();
        let col_global = rel.offset + ci;
        let col_ty = &rel.table.columns[ci].ty;
        for filter in &filters {
            if col_ty.range_element().is_some() {
                if let Some((strategy, _)) = gist_sibling_match(filter, col_global, rel.offset) {
                    return Some(ScanBound::Gist(GistBound {
                        name_key: idx.name.to_ascii_lowercase(),
                        strategy,
                        col_global,
                        scalar_type: None,
                    }));
                }
            } else if is_gist_scalar_type(col_ty)
                && gist_scalar_sibling_match(filter, col_global, rel.offset).is_some()
            {
                return Some(ScanBound::Gist(GistBound {
                    name_key: idx.name.to_ascii_lowercase(),
                    strategy: crate::gist::GistStrategy::Equal,
                    col_global,
                    scalar_type: Some(col_ty.scalar()),
                }));
            }
        }
    }
    for idx in &rel.table.indexes {
        if idx.kind != IndexKind::Gin {
            continue;
        }
        let ci = idx.first_column();
        let col_global = rel.offset + ci;
        let Some(elem_type) = rel.table.columns[ci].ty.array_element().map(|t| t.scalar()) else {
            continue;
        };
        for filter in &filters {
            if let Some((strategy, _)) = gin_sibling_match(filter, col_global, rel.offset) {
                return Some(ScanBound::Gin(GinBound {
                    name_key: idx.name.to_ascii_lowercase(),
                    elem_type,
                    strategy,
                    col_global,
                }));
            }
        }
    }
    None
}

/// The collation a key over `col` is STORED under, deciding whether — and how — a comparison bound
/// may push down to that key (spec/design/collation.md §8/§12). Three outcomes:
///   - `Some(None)`       — `col` is `C` (or non-text): the key is raw bytes (encoding.md §2.4),
///                          always pushable, the unchanged fast path.
///   - `Some(Some(coll))` — `col` is collated and the collation is `Full` (its file pin matches the
///                          loaded bundle): the key is the UCA sort key (encoding.md §2.12), pushable
///                          using `coll` to encode the probe in the same form.
///   - `None`             — `col` is collated but `Skewed` (the file's keys are at a DIFFERENT
///                          `(unicode, cldr)` than the loaded bundle provides): pushdown is REFUSED.
///                          The scan stays a full heap-scan that recomputes against the LOADED table
///                          (the read-safety rule §12; seeking a loaded-version probe in a
///                          file-version B-tree would mis-match — the regression tripwire
///                          suites/collation/skew.test stays green only because this refuses). An
///                          unresolvable collation likewise refuses rather than mis-encoding.
pub(crate) fn key_collation_ctx(
    catalog: &Engine,
    col: &Column,
) -> Option<Option<std::sync::Arc<Collation>>> {
    match &col.collation {
        None => Some(None),
        Some(name) => {
            let snap = catalog.read_snap();
            if snap.collation_skew(name).is_some() {
                None
            } else {
                snap.resolve_collation(name).map(Some)
            }
        }
    }
}

/// Whether a single base relation's `ORDER BY` is satisfied **by its primary-key scan order**
/// (spec/design/cost.md §3 "ORDER BY satisfied by primary-key order") — i.e. the table tree, walked
/// forward in storage-key order, already delivers rows in the requested order, so the sort is a
/// no-op. True iff the `ORDER BY` keys are a **prefix of the PK columns** (in key order), each
/// `ASC` (a `DESC` reverse scan is a follow-on) and sorting by the **same order the stored PK key
/// realizes** (collation.md §8/§12). The PK columns are NOT NULL, so a key's `NULLS FIRST|LAST` is
/// a no-op (no NULLs to place) and is ignored. Two coverage shapes both qualify: an `ORDER BY`
/// shorter than the PK is a prefix (ties are broken by the remaining PK columns — the canonical PK
/// tie-break, matching the eager stable sort); an `ORDER BY` longer than the PK matches the whole
/// PK and its extra keys are redundant (the PK is unique, so there are no ties left to break).
/// Reports whether a single base relation's `ORDER BY` is satisfied by its PRIMARY-KEY scan order
/// (spec/design/cost.md §3), and in which **direction** — `Some(false)` for a forward (`ASC`) scan,
/// `Some(true)` for a reverse (`DESC`) scan, `None` when the sort cannot be elided.
///
/// The direction is taken from the first `ORDER BY` key; every PK-prefix key must share it (a mixed
/// `ASC`/`DESC` order is no pure scan direction). Two asymmetric coverage rules, both grounded in the
/// eager sort being a **stable sort that breaks ties in input = PK-ascending order**:
/// - **Forward (`ASC`)** allows a strict **prefix** of the PK — the remaining PK columns tie-break
///   ascending, exactly the input order the stable sort preserves (so the forward scan's
///   continuation matches).
/// - **Reverse (`DESC`)** requires the **full PK** (`order.len() >= pk.len()`): a strict DESC prefix
///   of a composite PK would have the eager sort break ties in PK-**ascending** input order, which a
///   reverse scan inverts — so reverse is restricted to the unique full key, where no ties remain.
pub(crate) fn order_satisfied_by_pk(
    table: &Table,
    offset: usize,
    order: &[crate::spill::SortKey],
    catalog: &Engine,
) -> Option<bool> {
    let pk = table.pk_indices();
    if pk.is_empty() {
        return None; // no PK (synthetic rowid order is not a user-visible column)
    }
    let reverse = order[0].1; // direction comes from the first ORDER BY key's `descending` flag
    if reverse && order.len() < pk.len() {
        return None; // a reverse scan needs the full (unique) PK so no ties remain (see above)
    }
    let m = order.len().min(pk.len());
    for (i, (slot, descending, _nulls_first, coll)) in order.iter().take(m).enumerate() {
        if *descending != reverse {
            return None; // every PK-prefix key must share the scan direction (no mixed ASC/DESC)
        }
        if *slot != offset + pk[i] {
            return None; // must be the i-th PK column, in key order
        }
        // The ORDER BY key must sort by the SAME order the stored PK key realizes. A raw-byte
        // (`C`/non-text) key matches a key with no collation; a `Full`-collated key matches the
        // SAME collation; a `Skewed`/unresolvable collation never matches (the stored keys are at
        // the file's pinned version, so the scan order would be wrong for the loaded one — the
        // read-safety rule §12; recompute via the eager/streaming sort instead).
        match key_collation_ctx(catalog, &table.columns[pk[i]]) {
            None => return None,
            Some(None) => {
                if coll.is_some() {
                    return None;
                }
            }
            Some(Some(c)) => match coll {
                Some(c2) if c2.name == c.name => {}
                _ => return None,
            },
        }
    }
    Some(reverse)
}

/// Whether a frame folds only rows at or before the current row in the scan order (spec/design/
/// window.md §5.2/§6). The frame END must not look forward; a RANGE/GROUPS CURRENT-ROW end spans the
/// current peer group, which pulls in later rows unless the ordering key is unique. A ROWS frame uses
/// physical position, so it never expands to peers. The default frame (`None`, with a window ORDER BY)
/// is RANGE UNBOUNDED PRECEDING TO CURRENT ROW — safe only when the key is unique.
pub(crate) fn frame_backward_safe(frame: &Option<ResolvedFrame>, unique: bool) -> bool {
    let Some(frame) = frame else {
        return unique;
    };
    match &frame.end {
        // Strictly before the current peer group.
        ResolvedBound::UnboundedPreceding | ResolvedBound::Preceding(_) => true,
        // ROWS = the physical current row; RANGE/GROUPS = the current peer group (forward peers unless
        // the key is unique).
        ResolvedBound::CurrentRow => matches!(frame.mode, crate::ast::FrameMode::Rows) || unique,
        // Look forward.
        ResolvedBound::Following(_) | ResolvedBound::UnboundedFollowing => false,
    }
}

/// The fixed byte width of a table's stored primary key (`encode_pk_key` = the bare per-column
/// order-preserving keys concatenated, no NULL tags — a PK is `NOT NULL`), or `None` when ANY PK
/// column is variable-width (`text`/`decimal`/`bytea`/`interval`) or non-scalar (range/composite),
/// or the table has no PK. Used by the secondary-index-order scan to **peel the PK suffix off the
/// END of each index entry key** (the "key-suffix skip", cost.md §3) — sound only when that suffix
/// is a known fixed length, which is exactly when this returns `Some`.
pub(crate) fn pk_storage_width(table: &Table) -> Option<usize> {
    let pk = table.pk_indices();
    if pk.is_empty() {
        return None; // a no-PK table keys on a synthetic rowid — not handled this slice
    }
    let mut w = 0usize;
    for &ci in &pk {
        let s = table.columns[ci].ty.as_scalar()?; // a non-scalar (range/composite) PK has no fixed width
        if !s.is_fixed_width() {
            return None; // a variable-width (text/decimal/…) PK suffix is not a fixed peel
        }
        w += s.width_bytes();
    }
    Some(w)
}

/// The secondary-index-order plan: walk a B-tree index in key order to satisfy an `ORDER BY` without
/// a sort, point-looking-up each row by its primary key (cost.md §3 "secondary-index order").
pub(crate) struct IndexOrder {
    /// The index store's key — the lowercased index name.
    pub(crate) name_key: String,
    /// The fixed byte width of the PK suffix to peel off the END of each index entry key
    /// ([`pk_storage_width`]) — the row's storage key, fed to the table point lookup.
    pub(crate) pk_width: usize,
}

/// Reports whether a single base relation's `ORDER BY` is satisfied by walking one of its **B-tree
/// secondary indexes** in key order (cost.md §3 "secondary-index order"), and which index. The index
/// store holds its entries in `(indexed columns, storage key)` order, so a forward walk delivers rows
/// in `ORDER BY <indexed columns> ASC NULLS LAST` order, ties broken by the PK — exactly the eager
/// stable sort's tie-break.
///
/// Returns `Some` iff the `ORDER BY` keys are **exactly** a B-tree index's columns (same count, same
/// columns in key order), each `ASC` with **default `NULLS LAST`** (the index stores `NULL` as `0x01`
/// after a present `0x00`, so it realizes NULLS-LAST; an explicit `NULLS FIRST` does not match) and
/// sorting by the column's stored key collation (`Skewed`/unresolvable → refuse, the §12 read-safety
/// rule), **and** the table's PK is fixed-width ([`pk_storage_width`]). The exact-match requirement is
/// load-bearing: a strict prefix of a *multi*-column index would tie-break by the remaining index
/// columns rather than the PK, diverging from the eager sort (the same tie-break trap the
/// composite-PK reverse case carries). `DESC` (a reverse index walk) is a follow-on.
pub(crate) fn order_satisfied_by_index(
    table: &Table,
    offset: usize,
    order: &[crate::spill::SortKey],
    catalog: &Engine,
) -> Option<IndexOrder> {
    let pk_width = pk_storage_width(table)?;
    for idx in &table.indexes {
        if idx.kind != IndexKind::Btree {
            continue; // only an ordered B-tree realizes the column order (GIN/GiST do not)
        }
        // A PARTIAL index is not used for ORDER-BY skip-sort this slice (indexes.md §9): it holds
        // only its qualifying rows, so walking it would drop rows unless the query implies the
        // predicate — the predicate-implication gate lives only on the access-predicate bound. Stays
        // non-partial (a follow-on); falling through leaves a correct full-scan + sort.
        if idx.predicate.is_some() {
            continue;
        }
        // ORDER-BY skip-sort is column-only this slice (matching ORDER BY against an expression
        // index key is a follow-on — indexes.md §5).
        let Some(cols) = idx.column_ordinals() else {
            continue;
        };
        if order.len() != cols.len() {
            continue; // the ORDER BY must be EXACTLY the index columns (see the doc — tie-break)
        }
        let matches = order
            .iter()
            .enumerate()
            .all(|(i, (slot, descending, nulls_first, coll))| {
                if *descending || *nulls_first {
                    return false; // ASC + NULLS LAST only — the order a forward index walk realizes
                }
                if *slot != offset + cols[i] {
                    return false; // the i-th index column, in key order
                }
                match key_collation_ctx(catalog, &table.columns[cols[i]]) {
                    None => false, // Skewed / unresolvable — never walked for order (§12)
                    Some(None) => coll.is_none(),
                    Some(Some(c)) => matches!(coll, Some(c2) if c2.name == c.name),
                }
            });
        if matches {
            return Some(IndexOrder {
                name_key: idx.name.to_ascii_lowercase(),
                pk_width,
            });
        }
    }
    None
}

/// Inventory one GIN index when its array column has an accelerable conjunct (`col @> const`,
/// `col && const`, `const = ANY(col)`, or `col = const`). Legacy selection later picks a winner.
pub(crate) fn build_gin_bound_for_index(
    filter: &RExpr,
    idx: &IndexDef,
    columns: &[Column],
    offset: usize,
) -> Option<GinBound> {
    if idx.kind != IndexKind::Gin {
        return None;
    }
    let ci = idx.first_column();
    let col_global = offset + ci;
    let elem_ty = columns[ci].ty.array_element()?.scalar();
    if let Some((strategy, _)) = gin_match(filter, col_global) {
        return Some(GinBound {
            name_key: idx.name.to_ascii_lowercase(),
            elem_type: elem_ty,
            strategy,
            col_global,
        });
    }
    None
}

/// Inventory one single-column GiST index. Multi-column GiST indexes are EXCLUDE backing structures
/// and remain constraint-only, never planner candidates.
pub(crate) fn build_gist_bound_for_index(
    filter: &RExpr,
    idx: &IndexDef,
    columns: &[Column],
    offset: usize,
) -> Option<GistBound> {
    if idx.kind != IndexKind::Gist || idx.keys.len() != 1 {
        return None;
    }
    let ci = idx.first_column();
    let col_global = offset + ci;
    let col_ty = &columns[ci].ty;
    if col_ty.range_element().is_some() {
        if let Some((strategy, _)) = gist_match(filter, col_global) {
            return Some(GistBound {
                name_key: idx.name.to_ascii_lowercase(),
                strategy,
                col_global,
                scalar_type: None,
            });
        }
    } else if is_gist_scalar_type(col_ty) && gist_scalar_match(filter, col_global).is_some() {
        return Some(GistBound {
            name_key: idx.name.to_ascii_lowercase(),
            strategy: crate::gist::GistStrategy::Equal,
            col_global,
            scalar_type: Some(col_ty.scalar()),
        });
    }
    None
}

/// Find the first WHERE AND-chain conjunct that a GiST `range_ops` index on `col_global`
/// accelerates (spec/design/gist.md §5): `col && Q` (overlap — symmetric, the column may be either
/// operand) or `col @> Q` (contains — asymmetric, the column must be the LEFT operand; `Q @> col`
/// is the non-accelerated `<@`, gist.md §5). `Q` must be a **constant** (re-evaluable per scan, not
/// per row). The other range operators (`<@`/`<<`/`>>`/`&<`/`&>`/`-|-`/`=`) stay full-scan this
/// slice (gist.md §5). Returns the descent strategy and a reference to the constant query operand —
/// used at plan time (the strategy) and exec time (recover the operand from `plan.filter`), so the
/// two agree on the same conjunct by construction.
pub(crate) fn gist_match(
    filter: &RExpr,
    col_global: usize,
) -> Option<(crate::gist::GistStrategy, &RExpr)> {
    gist_match_operand(filter, col_global, rexpr_is_constant)
}

pub(crate) fn gist_sibling_match(
    filter: &RExpr,
    col_global: usize,
    cutoff: usize,
) -> Option<(crate::gist::GistStrategy, &RExpr)> {
    gist_match_operand(
        filter,
        col_global,
        |q| matches!(q, RExpr::Column(i) if *i < cutoff),
    )
}

fn gist_match_operand<'a, F>(
    filter: &'a RExpr,
    col_global: usize,
    query_ok: F,
) -> Option<(crate::gist::GistStrategy, &'a RExpr)>
where
    F: Fn(&RExpr) -> bool + Copy,
{
    use crate::gist::GistStrategy;
    match filter {
        RExpr::And(l, r) => gist_match_operand(l, col_global, query_ok)
            .or_else(|| gist_match_operand(r, col_global, query_ok)),
        // `col && Q` — overlap is symmetric in its operands.
        RExpr::RangeOp {
            op: RangeOp::Overlaps,
            args,
            ..
        } if args.len() == 2 => {
            if is_column(&args[0], col_global) && query_ok(&args[1]) {
                Some((GistStrategy::Overlaps, &args[1]))
            } else if is_column(&args[1], col_global) && query_ok(&args[0]) {
                Some((GistStrategy::Overlaps, &args[0]))
            } else {
                None
            }
        }
        // `col @> Q` — containment is asymmetric: the indexed column must be the container (LEFT).
        RExpr::RangeOp {
            op: RangeOp::Contains,
            args,
            ..
        } if args.len() == 2 => (is_column(&args[0], col_global) && query_ok(&args[1]))
            .then_some((GistStrategy::Contains, &args[1])),
        _ => None,
    }
}

/// Find the first WHERE AND-chain conjunct that a GiST scalar `=` opclass on `col_global`
/// accelerates (spec/design/gist.md §6): `col = Q` where `Q` is a **constant** (re-evaluable per
/// scan, not per row). Equality is commutative — the column may be either operand. `<>` and the
/// inequalities are not accelerated (a GiST `=` opclass has only the equal strategy). Returns the
/// `Equal` strategy and a reference to the constant operand (recovered at exec from `plan.filter`,
/// so plan and exec agree on the same conjunct by construction — the `gist_match` precedent).
pub(crate) fn gist_scalar_match(
    filter: &RExpr,
    col_global: usize,
) -> Option<(crate::gist::GistStrategy, &RExpr)> {
    gist_scalar_match_operand(filter, col_global, rexpr_is_constant)
}

pub(crate) fn gist_scalar_sibling_match(
    filter: &RExpr,
    col_global: usize,
    cutoff: usize,
) -> Option<(crate::gist::GistStrategy, &RExpr)> {
    gist_scalar_match_operand(
        filter,
        col_global,
        |q| matches!(q, RExpr::Column(i) if *i < cutoff),
    )
}

fn gist_scalar_match_operand<'a, F>(
    filter: &'a RExpr,
    col_global: usize,
    query_ok: F,
) -> Option<(crate::gist::GistStrategy, &'a RExpr)>
where
    F: Fn(&RExpr) -> bool + Copy,
{
    use crate::gist::GistStrategy;
    match filter {
        RExpr::And(l, r) => gist_scalar_match_operand(l, col_global, query_ok)
            .or_else(|| gist_scalar_match_operand(r, col_global, query_ok)),
        RExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
            ..
        } => {
            if is_column(lhs, col_global) && query_ok(rhs) {
                Some((GistStrategy::Equal, rhs.as_ref()))
            } else if is_column(rhs, col_global) && query_ok(lhs) {
                Some((GistStrategy::Equal, lhs.as_ref()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Recover a GiST bound's constant query operand from the live filter at exec time — `gist_match`
/// for `range_ops` (`&&`/`@>`), `gist_scalar_match` for the scalar `=` opclass. Centralizes the
/// strategy dispatch so every scan site (SELECT / UPDATE / DELETE) recovers the operand uniformly.
pub(crate) fn gist_query_operand<'a>(filter: &'a RExpr, gb: &GistBound) -> Option<&'a RExpr> {
    match gb.strategy {
        crate::gist::GistStrategy::Equal => {
            gist_scalar_match(filter, gb.col_global).map(|(_, q)| q)
        }
        _ => gist_match(filter, gb.col_global).map(|(_, q)| q),
    }
}

/// Find the first WHERE AND-chain conjunct that a GIN index on `col_global` accelerates
/// (spec/design/gin.md §6): `col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)`
/// (membership), or `col = Q` (exact array equality) where the query operand is a **constant**
/// (references no column / outer / subquery — re-evaluable per scan, not per row). `@>` is
/// asymmetric (the indexed column must be the LEFT operand — `Q @> col` is the non-accelerated
/// `<@`); `&&` and array `=` are symmetric (the column may be either operand). Returns the
/// strategy and a reference to the constant query operand. Used both at plan time (for the
/// strategy) and exec time (to recover the operand from `plan.filter`), so the two agree on the
/// same conjunct by construction.
pub(crate) fn gin_match(filter: &RExpr, col_global: usize) -> Option<(GinStrategy, &RExpr)> {
    gin_match_operand(filter, col_global, rexpr_is_constant)
}

pub(crate) fn gin_sibling_match(
    filter: &RExpr,
    col_global: usize,
    cutoff: usize,
) -> Option<(GinStrategy, &RExpr)> {
    gin_match_operand(
        filter,
        col_global,
        |q| matches!(q, RExpr::Column(i) if *i < cutoff),
    )
}

fn gin_match_operand<'a, F>(
    filter: &'a RExpr,
    col_global: usize,
    query_ok: F,
) -> Option<(GinStrategy, &'a RExpr)>
where
    F: Fn(&RExpr) -> bool + Copy,
{
    match filter {
        RExpr::And(l, r) => gin_match_operand(l, col_global, query_ok)
            .or_else(|| gin_match_operand(r, col_global, query_ok)),
        RExpr::ArrayFunc {
            func: ArrayFunc::Contains,
            args,
        } if args.len() == 2 => (is_column(&args[0], col_global) && query_ok(&args[1]))
            .then_some((GinStrategy::Contains, &args[1])),
        RExpr::ArrayFunc {
            func: ArrayFunc::Overlaps,
            args,
        } if args.len() == 2 => {
            if is_column(&args[0], col_global) && query_ok(&args[1]) {
                Some((GinStrategy::Overlaps, &args[1]))
            } else if is_column(&args[1], col_global) && query_ok(&args[0]) {
                Some((GinStrategy::Overlaps, &args[0]))
            } else {
                None
            }
        }
        // `col = Q` — exact array equality (gin.md §6). Commutative: the column may be either
        // operand, the constant array `Q` the other. Recovered query operand is `Q`; `gin_bound_rows`
        // reads it via `Equal` (the @>-superset gather + the residual `=`). `<>` is NOT matched
        // (only `CmpOp::Eq`). When the column is an array, the other constant operand is necessarily
        // an array too (resolve rejects an array/scalar `=`), so `Q` is always an array here.
        RExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
            ..
        } => {
            if is_column(lhs, col_global) && query_ok(rhs) {
                Some((GinStrategy::Equal, rhs.as_ref()))
            } else if is_column(rhs, col_global) && query_ok(lhs) {
                Some((GinStrategy::Equal, lhs.as_ref()))
            } else {
                None
            }
        }
        // `c = ANY(col)` — the array spelling of membership (gin.md §6): the GIN column must be
        // ANY's ARRAY operand and `c` (the scalar `lhs`) a constant. Only `= ANY` (not `= ALL`,
        // not any other comparison/quantifier — those are not a single-term posting gather). The
        // recovered query operand is the scalar `c`; `gin_bound_rows` reads it via `Member`.
        RExpr::Quantified {
            op: CmpOp::Eq,
            all: false,
            lhs,
            array,
        } if is_column(array, col_global) && query_ok(lhs) => {
            Some((GinStrategy::Member, lhs.as_ref()))
        }
        _ => None,
    }
}

/// Is `e` a reference to the column at global scope index `col_global`?
pub(crate) fn is_column(e: &RExpr, col_global: usize) -> bool {
    matches!(e, RExpr::Column(i) if *i == col_global)
}

/// Is `e` a **constant** expression — evaluable without a current/outer row (so its value is the
/// same for every scanned row, computable once)? False for any column, correlated outer column, or
/// subquery; true for literals, params, and pure operations over them. Used to admit a GIN query
/// operand `Q` (spec/design/gin.md §6: a constant query only this slice).
pub(crate) fn rexpr_is_constant(e: &RExpr) -> bool {
    match e {
        RExpr::Column(_) | RExpr::OuterColumn { .. } | RExpr::Subquery { .. } => false,
        // A DateClock is row-independent but EXECUTION-scoped (the statement clock + session
        // zone) — conservatively not a "constant", so no plan-time consumer ever evaluates it
        // without a live statement environment (date.md §6).
        RExpr::DateClock { .. } => false,
        RExpr::ConstInt(_)
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
        | RExpr::ConstNull
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::Param(_) => true,
        RExpr::Row(xs) | RExpr::Array { elems: xs, .. } => xs.iter().all(rexpr_is_constant),
        RExpr::Field { base, .. } => rexpr_is_constant(base),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            rexpr_is_constant(base)
                && subscripts
                    .iter()
                    .flat_map(subscript_bounds)
                    .all(rexpr_is_constant)
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => rexpr_is_constant(inner),
        RExpr::Neg { operand, .. } => rexpr_is_constant(operand),
        RExpr::Not(x) => rexpr_is_constant(x),
        RExpr::Casing { arg, .. } => rexpr_is_constant(arg),
        RExpr::AtTimeZone { zone, value, .. } => {
            rexpr_is_constant(zone) && rexpr_is_constant(value)
        }
        RExpr::DateTrunc { unit, value, zone } => {
            rexpr_is_constant(unit)
                && rexpr_is_constant(value)
                && zone.as_ref().is_none_or(|z| rexpr_is_constant(z))
        }
        RExpr::Extract { value, .. } => rexpr_is_constant(value),
        RExpr::DateConvert { inner, .. } => rexpr_is_constant(inner),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. }
        | RExpr::And(lhs, rhs)
        | RExpr::Or(lhs, rhs) => rexpr_is_constant(lhs) && rexpr_is_constant(rhs),
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => rexpr_is_constant(base) && rexpr_is_constant(arg),
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            rexpr_is_constant(a) && rexpr_is_constant(b)
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => rexpr_is_constant(operand),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .all(|(c, r)| rexpr_is_constant(c) && rexpr_is_constant(r))
                && rexpr_is_constant(els)
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
        | RExpr::JsonPathFn { args, .. } => args.iter().all(rexpr_is_constant),
        RExpr::JsonSqlFn { ctx, path, .. } => rexpr_is_constant(ctx) && rexpr_is_constant(path),
        RExpr::InValues { lhs, .. } => rexpr_is_constant(lhs),
        RExpr::Quantified { lhs, array, .. } => rexpr_is_constant(lhs) && rexpr_is_constant(array),
    }
}

/// A secondary-index entry key (spec/design/indexes.md §3): each indexed column as the
/// One key element of an index resolved for a statement's maintenance (spec/design/indexes.md §4):
/// a plain column (by ordinal — encoded from `columns[ord].ty` + `colls[ord]`), or a resolved
/// **expression** carrying its `RExpr`, its encoding [`Type`], and its collation (evaluated against
/// each row, unmetered, to yield the key value). Built once per statement by
/// [`Engine::resolve_index`](super::Engine) from an [`IndexDef`].
pub(crate) enum ResolvedKey {
    Column(usize),
    Expr(RExpr, Type, Option<std::sync::Arc<Collation>>),
}

/// An index resolved for one statement's maintenance: the def's identity (name / unique / kind)
/// plus its per-element [`ResolvedKey`]s. Owned (no borrow of the catalog) so the write paths can
/// mutate stores while holding it. GIN/GiST indexes are always plain-column (this slice), so their
/// entry builders read the ordinals back via [`ResolvedKey::Column`].
pub(crate) struct ResolvedIndex {
    pub name: String,
    pub unique: bool,
    pub kind: IndexKind,
    pub keys: Vec<ResolvedKey>,
    /// A **partial** index's resolved `WHERE predicate` (spec/design/indexes.md §9): evaluated
    /// against each row (unmetered, like a key expression), a row is indexed / constrained **only**
    /// when it is TRUE. `None` for an ordinary (full) index — every row is indexed.
    pub predicate: Option<RExpr>,
}

impl ResolvedIndex {
    /// The plain-column ordinals of a GIN/GiST index (always all columns, this slice).
    fn column_ordinals(&self) -> Vec<usize> {
        self.keys
            .iter()
            .map(|k| match k {
                ResolvedKey::Column(c) => *c,
                ResolvedKey::Expr(..) => unreachable!("GIN/GiST index keys are plain columns"),
            })
            .collect()
    }
}

/// The order-preserving key [`Type`] an index-expression result encodes under, or `None` when the
/// result type is not key-encodable (a composite / json / unknown result — `0A000` at CREATE INDEX).
/// Every scalar is keyable (encoding.md §2); a keyable-scalar-element array/range is too.
pub(crate) fn resolved_to_key_type(rt: &ResolvedType) -> Option<Type> {
    Some(match rt {
        ResolvedType::Int(st) | ResolvedType::Float(st) => Type::Scalar(*st),
        ResolvedType::Bool => Type::Scalar(ScalarType::Bool),
        ResolvedType::Text => Type::Scalar(ScalarType::Text),
        ResolvedType::Decimal => Type::Scalar(ScalarType::Decimal),
        ResolvedType::Bytea => Type::Scalar(ScalarType::Bytea),
        ResolvedType::Uuid => Type::Scalar(ScalarType::Uuid),
        ResolvedType::Timestamp => Type::Scalar(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Type::Scalar(ScalarType::Timestamptz),
        ResolvedType::Interval => Type::Scalar(ScalarType::Interval),
        ResolvedType::Date => Type::Scalar(ScalarType::Date),
        ResolvedType::Array(elem) => {
            let et = resolved_to_key_type(elem)?;
            if !is_keyable_scalar(&et) {
                return None; // a composite-element array is not keyable
            }
            Type::Array(Box::new(et))
        }
        ResolvedType::Range(elem) => Type::Range(Box::new(resolved_to_key_type(elem)?)),
        ResolvedType::Null
        | ResolvedType::Composite(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath => return None,
    })
}

/// One key element's value + encoding type + collation for a row: the column value (a column key)
/// or the evaluated expression (an expression key — unmetered, `env` for the immutable eval).
/// Index maintenance is unmetered (cost.md §3), so a throwaway [`Meter`] absorbs the eval charge.
fn index_key_slot<'a>(
    key: &'a ResolvedKey,
    columns: &'a [Column],
    colls: &'a [Option<std::sync::Arc<Collation>>],
    row: &Row,
    env: &EvalEnv,
) -> Result<(Value, &'a Type, Option<&'a Collation>)> {
    Ok(match key {
        ResolvedKey::Column(ci) => (row[*ci].clone(), &columns[*ci].ty, colls[*ci].as_deref()),
        ResolvedKey::Expr(rx, ty, coll) => {
            let mut m = Meter::new(); // maintenance eval is unmetered (cost.md §3)
            (rx.eval(row, env, &mut m)?, ty, coll.as_deref())
        }
    })
}

/// encoding.md §2.2 nullable slot — `0x00` + the type's bare order-preserving key bytes when
/// present, the lone `0x01` for NULL (always tagged, even for a NOT NULL column) — then the
/// row's storage key as the suffix. A column key's value is always resident (never `Unfetched`):
/// a fixed-width type never spills, and a `text`/`bytea` value large enough to spill would
/// produce an over-`RECORD_MAX` entry key, rejected `0A000` at the insert that stored it. An
/// expression key evaluates against the row (spec/design/indexes.md §4); a referenced spilled
/// value faults in through the evaluator's `Unfetched` backstop.
pub(crate) fn index_entry_key(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    rindex: &ResolvedIndex,
    storage_key: &[u8],
    row: &Row,
    env: &EvalEnv,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for key in &rindex.keys {
        let (val, ty, coll) = index_key_slot(key, columns, colls, row, env)?;
        match val {
            Value::Null => out.push(0x01),
            v => {
                // present tag, then the type's order-preserving key (range-aware §2.11,
                // collated-text-aware §2.12)
                out.push(0x00);
                out.extend_from_slice(&encode_typed_key(ty, &v, coll)?);
            }
        }
    }
    out.extend_from_slice(storage_key);
    Ok(out)
}

/// Whether a row is indexed by `rindex` (spec/design/indexes.md §9): always for an ordinary index,
/// and for a **partial** index iff its `WHERE predicate` evaluates to **TRUE** (the 3VL WHERE rule —
/// FALSE and NULL are excluded). The predicate eval is unmetered maintenance work (like a key
/// expression's — cost.md §3), so a throwaway [`Meter`] absorbs its charge.
fn index_row_qualifies(rindex: &ResolvedIndex, row: &Row, env: &EvalEnv) -> Result<bool> {
    match &rindex.predicate {
        None => Ok(true),
        Some(pred) => {
            let mut m = Meter::new(); // maintenance eval is unmetered (cost.md §3)
            Ok(matches!(pred.eval(row, env, &mut m)?, Value::Bool(true)))
        }
    }
}

/// The index entries a row contributes (spec/design/gin.md §4/§5): exactly one for an ordered
/// (B-tree) index — the §3 nullable-slot entry key — or one per DISTINCT non-NULL element for a
/// GIN index. Every write path (build, INSERT, DELETE, UPDATE) treats an index uniformly as "a
/// row maps to a set of entries." A **partial** index contributes the **empty** set for a row whose
/// predicate is not TRUE (spec/design/indexes.md §9), which is what makes INSERT/DELETE/UPDATE
/// maintenance uniform (the UPDATE old-set/new-set diff handles a row entering/leaving/moving for
/// free). `colls` (column-ordinal-indexed) selects each text key column's collated form (§2.12);
/// GIN elements are fixed-width, so a GIN index never collates. `env` evaluates any expression key
/// element or the partial predicate (B-tree only — GIN/GiST are plain-column, this slice).
pub(crate) fn index_entry_keys(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    rindex: &ResolvedIndex,
    storage_key: &[u8],
    row: &Row,
    env: &EvalEnv,
) -> Result<Vec<Vec<u8>>> {
    if !index_row_qualifies(rindex, row, env)? {
        return Ok(Vec::new()); // partial index: a non-qualifying row contributes no entry
    }
    Ok(match rindex.kind {
        IndexKind::Btree => vec![index_entry_key(
            columns,
            colls,
            rindex,
            storage_key,
            row,
            env,
        )?],
        IndexKind::Gin => gin_entries(columns, &rindex.column_ordinals(), storage_key, row),
        IndexKind::Gist => gist_entries(columns, &rindex.column_ordinals(), storage_key, row),
    })
}

/// Entry keys for a COLUMN-ONLY index, without an eval env (spec/design/indexes.md §4) — the
/// collation-realign rebuild path, which runs on a `Snapshot` with no `Engine` to evaluate an
/// expression key. An expression index is C-collated (its keys never change on a collation
/// upgrade — indexes.md §1), so it is rebuilt at the Engine level instead; this asserts the
/// index is plain-column.
pub(crate) fn index_entry_keys_columns(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    def: &IndexDef,
    storage_key: &[u8],
    row: &Row,
) -> Result<Vec<Vec<u8>>> {
    let cols = def
        .column_ordinals()
        .expect("index_entry_keys_columns called on an expression index");
    Ok(match def.kind {
        IndexKind::Btree => {
            let mut out = Vec::new();
            for &ci in &cols {
                match &row[ci] {
                    Value::Null => out.push(0x01),
                    v => {
                        out.push(0x00);
                        out.extend_from_slice(&encode_typed_key(
                            &columns[ci].ty,
                            v,
                            colls[ci].as_deref(),
                        )?);
                    }
                }
            }
            out.extend_from_slice(storage_key);
            vec![out]
        }
        IndexKind::Gin => gin_entries(columns, &cols, storage_key, row),
        IndexKind::Gist => gist_entries(columns, &cols, storage_key, row),
    })
}

/// A GiST index's entry keys for one row (spec/design/gist.md §4.1/§7): exactly one leaf key, the
/// per-column component bounds concatenated then `‖ storage_key` ([`gist::leaf_key_multi`]) — the
/// GIN `term ‖ skey` pattern, so all existing index maintenance (insert/update/delete) reuses it
/// unchanged. A single-column GX1/GX2 index has one component; an EXCLUDE backing index one per
/// `WITH` column. A NULL in **any** indexed column produces NO entry (the §7 exclusion NULL rule —
/// a row with a NULL excluded column never conflicts and is left out of the tree; the GIN NULL-skip
/// precedent). The empty range is a real value and IS indexed.
pub(crate) fn gist_entries(
    columns: &[Column],
    cols: &[usize],
    storage_key: &[u8],
    row: &Row,
) -> Vec<Vec<u8>> {
    // Pre-encode scalar key bytes so the borrowed `GistLeafComp::Scalar(&[u8])` outlives the build.
    let mut scalar_keys: Vec<Vec<u8>> = Vec::new();
    for &ci in cols {
        let col = &columns[ci];
        if matches!(row[ci], Value::Null) {
            return Vec::new(); // any NULL excluded column → row not indexed (NULL rule)
        }
        if col.ty.range_element().is_none() {
            // scalar `=` opclass: the value's order-preserving KEY bytes (gist.md §6). The column
            // is a FIXED-WIDTH keyable (the gate), so the key encoding is collation-free/infallible.
            let k = encode_key_value(col.ty.scalar(), &row[ci], None)
                .expect("a fixed-width GiST scalar key is infallible (no collation)");
            scalar_keys.push(k);
        }
    }
    let mut comps: Vec<crate::gist::GistLeafComp> = Vec::with_capacity(cols.len());
    let mut next_scalar = 0usize;
    for &ci in cols {
        let col = &columns[ci];
        match col.ty.range_element() {
            Some(elem) => match &row[ci] {
                Value::Range(rv) => comps.push(crate::gist::GistLeafComp::Range(elem.scalar(), rv)),
                _ => unreachable!("a GiST range index column holds a range or NULL"),
            },
            None => {
                comps.push(crate::gist::GistLeafComp::Scalar(&scalar_keys[next_scalar]));
                next_scalar += 1;
            }
        }
    }
    vec![crate::gist::leaf_key_multi(&comps, storage_key)]
}

/// Build a row's `EXCLUDE` conjunction probe (spec/design/gist.md §7): one GiST query operand +
/// strategy per excluded column, in the backing index's column order. Returns `None` (the row is
/// **exempt**, never conflicts) when the **NULL rule** fires (any excluded column is NULL) or when a
/// `&&` element holds the **empty range** (`empty && anything` is FALSE, so the conjunction can
/// never be TRUE — this also sidesteps the empty-range overlap-descend trap, gist.md §5). The query
/// is fed to the resident GiST tree's `search`, whose leaf recheck IS the full conjunction, so a hit
/// is a genuine conflict.
pub(crate) fn exclusion_probe_query(
    columns: &[Column],
    exc: &ExclusionConstraint,
    row: &Row,
) -> Option<(Vec<crate::gist::GistQuery>, Vec<crate::gist::GistStrategy>)> {
    use crate::gist::{GistQuery, GistStrategy};
    let mut q = Vec::with_capacity(exc.elements.len());
    let mut strats = Vec::with_capacity(exc.elements.len());
    for el in &exc.elements {
        let ci = el.column;
        match (&row[ci], el.op) {
            (Value::Null, _) => return None, // NULL rule: exempt
            (Value::Range(rv), ExclusionOp::Overlaps) => {
                if rv.empty {
                    return None; // empty && anything is FALSE → exempt
                }
                q.push(GistQuery::Range(rv.clone()));
                strats.push(GistStrategy::Overlaps);
            }
            (v, ExclusionOp::Equal) => {
                let key = encode_key_value(columns[ci].ty.scalar(), v, None)
                    .expect("a fixed-width GiST scalar key is infallible (no collation)");
                q.push(GistQuery::Scalar(key));
                strats.push(GistStrategy::Equal);
            }
            _ => unreachable!("an && exclusion column holds a range or NULL"),
        }
    }
    Some((q, strats))
}

/// Does the `(expr_i op_i)` conjunction hold between two rows (spec/design/gist.md §7)? Used for the
/// in-batch new-row-vs-new-row check (the resident GiST tree holds only stored rows). A NULL in any
/// excluded column of either row, or an empty range under `&&` (`range_overlaps` of an empty range
/// is FALSE), makes that element not-TRUE → no conflict. Returns `true` only when EVERY element is
/// definitely TRUE.
pub(crate) fn exclusion_pair_conflicts(
    columns: &[Column],
    exc: &ExclusionConstraint,
    a: &Row,
    b: &Row,
) -> bool {
    for el in &exc.elements {
        let ci = el.column;
        let (va, vb) = (&a[ci], &b[ci]);
        if matches!(va, Value::Null) || matches!(vb, Value::Null) {
            return false;
        }
        let ok = match el.op {
            ExclusionOp::Overlaps => match (va, vb) {
                (Value::Range(ra), Value::Range(rb)) => crate::range::range_overlaps(ra, rb),
                _ => unreachable!("an && exclusion column holds a range or NULL"),
            },
            ExclusionOp::Equal => {
                let ka = encode_key_value(columns[ci].ty.scalar(), va, None)
                    .expect("a fixed-width GiST scalar key is infallible");
                let kb = encode_key_value(columns[ci].ty.scalar(), vb, None)
                    .expect("a fixed-width GiST scalar key is infallible");
                ka == kb
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Is `elem` an element type a GIN (`array_ops`) index admits? The integers, `boolean`, `uuid`,
/// `date`, `timestamp`, `timestamptz` (spec/design/gin.md §3): the GIN term IS the element's
/// order-preserving key encoding (§4) and a term carries no length/terminator framing, so only the
/// FIXED-WIDTH keyables qualify. The VARIABLE-width keyables (`text`, `bytea`, `decimal`) — though
/// valid ordered-index / PK keys — are 0A000 here, as is `float`. `interval` is fixed-width keyable
/// (its 16-byte span key landed this slice, encoding.md §2.10) but its GIN *element* support is a
/// separate follow-on slice (gin.md §3/§10 — like each element type before it), so it is not yet
/// admitted here.
pub(crate) fn is_gin_element_type(elem: &Type) -> bool {
    elem.is_integer()
        || elem.is_bool()
        || elem.is_uuid()
        || elem.is_timestamp()
        || elem.is_timestamptz()
        || elem.is_date()
}

/// Does the scalar `=` GiST opclass admit this column type (spec/design/gist.md §6)? The FIXED-WIDTH
/// keyables — integers, boolean, uuid, date, timestamp, timestamptz — whose bound is `[min, max]`
/// over the order-preserving key encoding, compared as raw bytes (no decode, no collation). Exactly
/// `is_gin_element_type`'s set (both stage on the fixed-width key-encodable scalars), kept a separate
/// predicate so the two surfaces evolve independently.
pub(crate) fn is_gist_scalar_type(ty: &Type) -> bool {
    ty.is_integer()
        || ty.is_bool()
        || ty.is_uuid()
        || ty.is_timestamp()
        || ty.is_timestamptz()
        || ty.is_date()
}

/// A keyable scalar the GiST scalar `=` opclass will eventually admit but defers this slice
/// (spec/design/gist.md §6/§11): the VARIABLE-width / collation-sensitive keyables — `text`,
/// `bytea`, `decimal`, `interval`. A column of one of these is `0A000` ("not supported yet"), not
/// `42704` (it is on the roadmap, like each GIN element type before it).
pub(crate) fn is_gist_deferred_scalar_type(ty: &Type) -> bool {
    ty.is_text() || ty.is_bytea() || ty.is_decimal() || ty.is_interval()
}

/// A GIN index's entry keys for one row (spec/design/gin.md §4): one entry per DISTINCT non-NULL
/// array element — `encode_element(term) ‖ storage_key`, with NO presence tag (a term is never
/// NULL) and an empty payload. A NULL array column value and an empty array both yield no entries
/// (so they never appear in any posting list — correct for `@>`/`&&`). Returned sorted by encoded
/// term (= key-encoding byte order, which is order-preserving for every admitted element type), so
/// the per-row order is deterministic. `array_ops` over any fixed-width key-encodable element type.
pub(crate) fn gin_entries(
    columns: &[Column],
    cols: &[usize],
    storage_key: &[u8],
    row: &Row,
) -> Vec<Vec<u8>> {
    let ci = cols[0];
    let elem_ty = columns[ci]
        .ty
        .array_element()
        .expect("a GIN index column is an array (CREATE INDEX gate)")
        .scalar();
    let mut terms: Vec<Vec<u8>> = Vec::new();
    if let Value::Array(arr) = &row[ci] {
        for el in &arr.elements {
            // a NULL element contributes no term; a non-keyable element is impossible under the gate
            if !matches!(el, Value::Null) {
                // a GIN element is fixed-width (is_gin_element_type excludes text), so it never
                // collates and the key encoding is infallible.
                terms.push(
                    encode_key_value(elem_ty, el, None)
                        .expect("a GIN element key is infallible (fixed-width, no collation)"),
                );
            }
        }
    }
    // Dedup by the encoded term: the encoding is a bijection, so byte-dedup == value-dedup, and
    // byte-sort == value-sort (order-preserving). Each distinct term yields one entry.
    terms.sort_unstable();
    terms.dedup();
    terms
        .into_iter()
        .map(|mut entry| {
            entry.extend_from_slice(storage_key);
            entry
        })
        .collect()
}

/// A row's PRIMARY-KEY STORAGE KEY (spec/design/encoding.md §2.3): the concatenation of the
/// members' order-preserving encodings in key order. Every keyable type is self-delimiting (the
/// scalars fixed-width or `0x00`-terminated, a `range` container framed §2.11), so the
/// concatenation is self-delimiting and `memcmp` equals the tuple's logical order. Each member is
/// encoded by the shared range-aware [`encode_typed_key`] (so a range PK member recurses into the
/// element codec, encoding.md §2.11); the tuple carries each member's full `Type` for that reason.
/// Shared by the INSERT duplicate check and the ON CONFLICT arbiter probe (spec/design/upsert.md §3);
/// a PK column is NOT NULL, so there is no presence tag and no NULL arm. `float`/`composite`/`array`
/// PKs are rejected at CREATE TABLE, so those value kinds never reach here. `colls`
/// (column-ordinal-indexed) selects a text PK member's collated form (§2.12); a non-`C` collated
/// member can fail the sort-key build (`0A000`), propagated here.
pub(crate) fn encode_pk_key(
    pk: &[(usize, Type)],
    colls: &[Option<std::sync::Arc<Collation>>],
    row: &Row,
) -> Result<Vec<u8>> {
    let mut k = Vec::new();
    for (i, pk_ty) in pk {
        k.extend_from_slice(&encode_typed_key(pk_ty, &row[*i], colls[*i].as_deref())?);
    }
    Ok(k)
}

/// A row's UNIQUENESS PROBE KEY for one unique index (spec/design/indexes.md §8): the §3
/// entry key's slot prefix — without the storage-key suffix — or `None` when any component
/// is NULL (*NULLS DISTINCT*: such a tuple never conflicts). Two rows conflict iff they
/// yield the same `Some` prefix. `colls` (column-ordinal-indexed) selects each text column's
/// collated form (§2.12).
pub(crate) fn index_prefix_key(
    columns: &[Column],
    colls: &[Option<std::sync::Arc<Collation>>],
    rindex: &ResolvedIndex,
    row: &Row,
    env: &EvalEnv,
) -> Result<Option<Vec<u8>>> {
    // A partial index constrains only its qualifying rows (indexes.md §9): a non-qualifying row is
    // exempt from uniqueness, exactly like a NULL-bearing prefix (returns `None`).
    if !index_row_qualifies(rindex, row, env)? {
        return Ok(None);
    }
    let mut out = Vec::new();
    for key in &rindex.keys {
        let (val, ty, coll) = index_key_slot(key, columns, colls, row, env)?;
        match val {
            Value::Null => return Ok(None),
            v => {
                // present tag, then the type's order-preserving key (range-aware §2.11,
                // collated-text-aware §2.12)
                out.push(0x00);
                out.extend_from_slice(&encode_typed_key(ty, &v, coll)?);
            }
        }
    }
    Ok(Some(out))
}

/// The half-open byte range `[prefix, byte-successor(prefix))` — every index entry whose
/// slot prefix equals `prefix` (the suffix makes tree keys unique, so equal prefixes sit
/// adjacent). The uniqueness probes range over it (spec/design/indexes.md §8).
pub(crate) fn unique_probe_bound(prefix: &[u8]) -> KeyBound {
    KeyBound {
        lo: Some(prefix.to_vec()),
        lo_inc: true,
        hi: prefix_successor(prefix),
        hi_inc: false,
    }
}

/// The byte-successor of a prefix: the smallest byte string greater than every string that
/// extends `p`. Increment the last non-0xFF byte and truncate after it; an all-0xFF prefix
/// has no successor (`None` ⇒ unbounded high end).
pub(crate) fn prefix_successor(p: &[u8]) -> Option<Vec<u8>> {
    let mut s = p.to_vec();
    while let Some(last) = s.last_mut() {
        if *last == 0xFF {
            s.pop();
        } else {
            *last += 1;
            return Some(s);
        }
    }
    None
}

/// The order-preserving key bytes for one keyable value (encoding.md §2), matching the PK / index
/// encoders. `value` is non-NULL and of a keyable type (a foreign-key column always is — its type
/// equals a PK/UNIQUE parent column, CREATE TABLE §6.2). `coll` is the text component's frozen
/// collation: `None` (the fast path, and every non-text type) keys a `text` by its raw UTF-8
/// (`text-terminated-escape` §2.4); `Some(c)` keys it by the collation's UCA sort key
/// (`text-collated-sortkey` §2.12), which can fail (`0A000`) on a code point the collation does not
/// map — propagated, so a collated INSERT of an unmapped string aborts the write.
pub(crate) fn encode_key_value(
    ty: ScalarType,
    value: &Value,
    coll: Option<&Collation>,
) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Int(n) => encode_int(ty, *n),
        Value::Bool(b) => encode_bool(*b),
        Value::Uuid(u) => u.to_vec(),
        Value::Timestamp(m) | Value::Timestamptz(m) => encode_int(ty, *m),
        Value::Date(d) => encode_int(ty, *d as i64),
        Value::Text(s) => match coll {
            Some(c) => collation::sort_key(c, s)?,
            None => encode_terminated(s.as_bytes()),
        },
        Value::Bytea(b) => encode_terminated(b),
        Value::Decimal(d) => d.encode_key(),
        Value::Interval(iv) => iv.encode_key(),
        Value::Float64(f) => encode_f64_key(*f),
        Value::Float32(f) => encode_f32_key(*f),
        _ => unreachable!("a foreign-key column is a key-encodable type (CREATE TABLE §6.2 gate)"),
    })
}

/// The `float-order-preserving` key body for an `f64` (encoding.md §2.8): canonicalize via
/// [`canon_f64_bits`] (`-0 → +0`, every NaN → one quiet pattern), take the bits big-endian, then
/// **if the sign bit is set flip all 64 bits, else flip just the sign bit** — mapping the binary64
/// total order (§3, `-Inf < finite < +Inf < NaN`) onto unsigned byte order. Fixed 8 bytes, so
/// self-delimiting by width (no escape/terminator). `-0`/`+0` and any two NaNs canonicalize to one
/// key, so a `UNIQUE` float key treats them as one. Infallible.
pub(crate) fn encode_f64_key(f: f64) -> Vec<u8> {
    let mut bits = crate::value::canon_f64_bits(f);
    bits ^= if bits >> 63 == 1 { u64::MAX } else { 1 << 63 };
    bits.to_be_bytes().to_vec()
}

/// As [`encode_f64_key`], for `f32` (binary32, 4 bytes — the `float-order-preserving` rule §2.8).
pub(crate) fn encode_f32_key(f: f32) -> Vec<u8> {
    let mut bits = crate::value::canon_f32_bits(f);
    bits ^= if bits >> 31 == 1 { u32::MAX } else { 1 << 31 };
    bits.to_be_bytes().to_vec()
}

/// The order-preserving key bytes for one keyable value given its column **`Type`** — the
/// range-aware encoder threaded through every key path (PK, index entry/prefix, FK probe). A range
/// recurses into the `range-bounds` container codec (encoding.md §2.11), pulling its element scalar
/// from the column type; every other keyable value ignores the wrapper and dispatches on its scalar
/// via [`encode_key_value`]. `value` is non-NULL (callers handle the NULL slot tag separately), and
/// a range column always holds a `Value::Range`, so the scalar arm never sees a range type. `coll`
/// selects a `text` column's key form (encoding.md §2.12); it never applies to a range element (no
/// range subtype is text).
pub(crate) fn encode_typed_key(
    ty: &Type,
    value: &Value,
    coll: Option<&Collation>,
) -> Result<Vec<u8>> {
    match value {
        Value::Range(rv) => {
            let elem = ty
                .range_element()
                .expect("a range key value has a range column type")
                .scalar();
            Ok(crate::range::encode_range_key(elem, rv))
        }
        Value::Array(a) => {
            let elem = ty
                .array_element()
                .expect("an array key value has an array column type");
            encode_array_key(elem, a)
        }
        _ => encode_key_value(ty.scalar(), value, coll),
    }
}

/// Whether `ty` is an **array** whose element is a key-encodable scalar — so the array is a valid
/// `PRIMARY KEY` / index / `UNIQUE` / FK key (encoding.md §2.14, the `array-elements-terminated` rule).
/// A `float`-element array (`f64[]`/`f32[]`) IS keyable (the §2.8 narrowing lifted — a float at rest is
/// in-contract); only a composite-element array (composite is not yet keyable) is NOT keyable, the same
/// narrowing the bare composite scalar key carries.
pub(crate) fn is_array_keyable(ty: &Type) -> bool {
    ty.array_element().is_some_and(is_keyable_scalar)
}

/// Whether `ty` is a key-encodable **scalar** — the element-type gate for [`is_array_keyable`].
/// Mirrors the inline scalar gate the PK/UNIQUE/index resolvers apply directly. With `float` keys
/// exercised (§2.8) every scalar is keyable, so this is the full keyable-scalar set; only the
/// recursive `composite` container is excluded (it has no value-kind here — a composite element
/// would arrive as `Type::Composite`, which none of these predicates match).
pub(crate) fn is_keyable_scalar(ty: &Type) -> bool {
    ty.is_integer()
        || ty.is_bool()
        || ty.is_text()
        || ty.is_bytea()
        || ty.is_decimal()
        || ty.is_uuid()
        || ty.is_timestamp()
        || ty.is_timestamptz()
        || ty.is_date()
        || ty.is_interval()
        || ty.is_float()
}

/// The order-preserving `array-elements-terminated` key for an array value (encoding.md §2.14) — the
/// engine's second container key, recursing into each element's own key. Reproduces the in-memory
/// `array_total_cmp` order (array.md §5) under `memcmp`: per flattened (row-major) element a marker
/// (`0x01` present ‖ the element key, `0x02` NULL) so present sorts before NULL and a shorter list
/// reaches the `0x00` terminator first; then the shape suffix (`ndim`, then per dimension a `u32` BE
/// length and the `i32` `int-be-signflip` lower bound) breaks ties among equal-element-prefix,
/// equal-count arrays. The element is a key-encodable **scalar** (`float` elements included since the
/// §2.8 lift; the DDL gate rejects only a composite element `0A000`), so the per-element key is
/// [`encode_key_value`]; an array element key uses the `C` byte order (a collated array-element key is
/// not a feature this slice).
pub(crate) fn encode_array_key(elem_ty: &Type, a: &ArrayVal) -> Result<Vec<u8>> {
    let elem = elem_ty.scalar();
    let mut out = Vec::new();
    for e in &a.elements {
        match e {
            Value::Null => out.push(0x02), // NULL element — sorts after every present element
            v => {
                out.push(0x01); // present element marker
                out.extend_from_slice(&encode_key_value(elem, v, None)?);
            }
        }
    }
    out.push(0x00); // terminator — a shorter element list sorts before a longer one
    out.push(a.ndim() as u8);
    for d in 0..a.ndim() {
        out.extend_from_slice(&(a.dims[d] as u32).to_be_bytes());
        out.extend_from_slice(&encode_int(ScalarType::Int32, a.lbounds[d] as i64));
    }
    Ok(out)
}

/// A built foreign-key probe (spec/design/constraints.md §6.4/§6.8): the bytes to look up in the
/// parent, tagged with which physical tree to probe.
pub(crate) enum FkProbe {
    /// The parent's PK storage key (bare member encodings concatenated, in PK key order).
    Pk(Vec<u8>),
    /// A parent unique index's prefix (0x00-tagged slots, in index-key order) + the lowercased
    /// index name.
    Unique { index: String, prefix: Vec<u8> },
}

impl FkProbe {
    /// The raw probe bytes — used to compare against this statement's batch end state (§6.4). Two
    /// probes of one FK share the same byte space (a given FK always probes the PK or always a
    /// fixed unique index), so byte equality is a valid set membership test.
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            FkProbe::Pk(b) => b,
            FkProbe::Unique { prefix, .. } => prefix,
        }
    }
}

/// Build the parent-key probe for `fk` from `row`, taking each referenced parent column's value
/// from `row[ordinals[i]]` where `ordinals[i]` supplies `fk.ref_columns[i]`. So the child side
/// passes `ordinals = &fk.columns` (local columns), and a self-reference batch entry passes
/// `ordinals = &fk.ref_columns` (the row viewed as a parent). Returns `None` when any supplied
/// value is NULL (MATCH SIMPLE exempt — §6.3). The probe uses the parent's PK when the referenced
/// set is the PK, else the matching unique index (re-derived deterministically — §6.8).
pub(crate) fn fk_probe(
    fk: &ForeignKeyConstraint,
    parent: &Table,
    parent_colls: &[Option<std::sync::Arc<Collation>>],
    row: &Row,
    ordinals: &[usize],
) -> Result<Option<FkProbe>> {
    // MATCH SIMPLE: a NULL in any supplied (local/parent) column exempts the whole tuple.
    if ordinals.iter().any(|&o| matches!(row[o], Value::Null)) {
        return Ok(None);
    }
    // The value supplying parent column `pcol` (the fk pairing: ref_columns[i] ⇄ ordinals[i]).
    let value_for = |pcol: usize| -> &Value {
        let i = fk
            .ref_columns
            .iter()
            .position(|&r| r == pcol)
            .expect("a parent key column is one of the FK's referenced columns");
        &row[ordinals[i]]
    };
    // The probe must match the PARENT's stored key, so a collated parent key column is encoded with
    // the PARENT's collation (encoding.md §2.12), independent of the child column's own collation.
    let ref_set = sorted_unique(&fk.ref_columns);
    if !parent.pk.is_empty() && sorted_unique(&parent.pk) == ref_set {
        let mut k = Vec::new();
        for &pcol in &parent.pk {
            k.extend_from_slice(&encode_typed_key(
                &parent.columns[pcol].ty,
                value_for(pcol),
                parent_colls[pcol].as_deref(),
            )?);
        }
        Ok(Some(FkProbe::Pk(k)))
    } else {
        let idx = parent
            .indexes
            .iter()
            .find(|i| {
                i.unique
                    && i.column_ordinals()
                        .is_some_and(|c| sorted_unique(&c) == ref_set)
            })
            .expect("referenced columns matched a unique key at CREATE TABLE §6.2");
        let idx_cols = idx
            .column_ordinals()
            .expect("FK target is a plain-column unique index");
        let mut prefix = Vec::new();
        for &pcol in &idx_cols {
            prefix.push(0x00);
            prefix.extend_from_slice(&encode_typed_key(
                &parent.columns[pcol].ty,
                value_for(pcol),
                parent_colls[pcol].as_deref(),
            )?);
        }
        Ok(Some(FkProbe::Unique {
            index: idx.name.to_ascii_lowercase(),
            prefix,
        }))
    }
}

/// Construct a PK tuple's maximal equality prefix plus optional range on the next member. Each
/// filter is walked as a top-level AND chain; ordinary scans pass WHERE, while INL passes ON+WHERE.
pub(crate) fn detect_pk_bound(
    filters: &[&RExpr],
    rel: &ScopeRel,
    sibling_cutoff: Option<usize>,
    catalog: &Engine,
) -> Option<PkBound> {
    let pk = rel.table.pk_indices();
    if pk.is_empty() {
        return None;
    }
    let mut eq_cols = Vec::new();
    let mut range = None;
    for ci in &pk {
        let Some(ty) = rel.table.columns[*ci].ty.as_scalar() else {
            break;
        };
        let Some(coll) = key_collation_ctx(catalog, &rel.table.columns[*ci]) else {
            break;
        };
        let mut terms = Vec::new();
        for filter in filters {
            collect_bound_terms(
                filter,
                &KeyMatch::Column(rel.offset + *ci),
                ty,
                coll.as_ref().map(|c| c.name.as_str()),
                sibling_cutoff,
                &mut terms,
            );
        }
        let mut eqs = Vec::new();
        let mut ranges = Vec::new();
        for term in terms {
            if matches!(term.op, CmpOp::Eq) {
                eqs.push(term.src);
            } else {
                ranges.push(term);
            }
        }
        if !eqs.is_empty() {
            eq_cols.push(PkEqCol {
                name: rel.table.columns[*ci].name.clone(),
                col_type: ty,
                srcs: eqs,
                ranges,
                coll,
            });
            continue;
        }
        if !ranges.is_empty() {
            range = Some(PkRange {
                name: rel.table.columns[*ci].name.clone(),
                col_type: ty,
                terms: ranges,
                coll,
            });
        }
        break;
    }
    (!eq_cols.is_empty() || range.is_some()).then(|| PkBound {
        eq_cols,
        range,
        member_count: pk.len(),
    })
}

/// `sibling_cutoff` (index-nested-loop join, cost.md §3 "JOIN"): when `Some(cut)`, a bare column
/// reference whose GLOBAL index is `< cut` — an EARLIER join relation's column — is a valid bound
/// source (`BoundSrc::Sibling`), resolved per outer row from the combined left-hand row. `None`
/// (the ordinary once-materialized bound) accepts only literals/params/outer references.
/// What a bound's key operand is (spec/design/indexes.md §5): a plain column at a global ordinal
/// (the PK bound and a column index key), or a resolved index EXPRESSION matched structurally
/// against a WHERE conjunct operand (an expression index key). For the expression form, the key's
/// `Column(i)` is table-local and matches a WHERE `Column(i + offset)`.
pub(crate) enum KeyMatch<'a> {
    Column(usize),
    Expr(&'a RExpr, usize),
}

impl KeyMatch<'_> {
    fn matches(&self, x: &RExpr) -> bool {
        match self {
            KeyMatch::Column(idx) => matches!(x, RExpr::Column(i) if *i == *idx),
            KeyMatch::Expr(e, offset) => rexpr_eq_shifted(x, e, *offset),
        }
    }
}

/// Does the WHERE `filter` imply a PARTIAL index's predicate (spec/design/indexes.md §9)? jed's
/// syntactic test (PG's, not a prover): the filter's top-level AND-chain must contain a conjunct
/// STRUCTURALLY EQUAL to the resolved predicate. `pred` is in table-local column coords; a `filter`
/// conjunct is global, so it is matched shifted by `offset` (the relation's global column base).
/// Sound-if-conservative: a miss means the index is not used (a correct full scan + residual filter).
pub(crate) fn filter_implies_predicate(filter: &RExpr, pred: &RExpr, offset: usize) -> bool {
    // The filter contains a top-level conjunct structurally equal to `target`.
    fn contains_conjunct(filter: &RExpr, target: &RExpr, offset: usize) -> bool {
        match filter {
            RExpr::And(l, r) => {
                contains_conjunct(l, target, offset) || contains_conjunct(r, target, offset)
            }
            conjunct => rexpr_eq_shifted(conjunct, target, offset),
        }
    }
    // Every top-level conjunct of `pred` must be present as a conjunct of `filter` (so a conjunctive
    // predicate `a AND b` is implied by a WHERE that lists both `a` and `b`, not only the whole `a AND b`).
    match pred {
        RExpr::And(l, r) => {
            filter_implies_predicate(filter, l, offset)
                && filter_implies_predicate(filter, r, offset)
        }
        single => contains_conjunct(filter, single, offset),
    }
}

/// A SOUND-if-incomplete structural equality for index-expression matching (spec/design/indexes.md
/// §5): does the WHERE conjunct operand `a` (GLOBAL column indices) equal the resolved index key
/// expression `b` (table-local `Column(i)`, matched as `Column(i + offset)`)? Covers the common
/// index-expression shapes; any unrecognized / typmod-bearing shape returns `false` — a missed
/// bound is always sound (a full scan + residual filter), matching PostgreSQL's syntactic (not
/// semantic) index-expression matching.
pub(crate) fn rexpr_eq_shifted(a: &RExpr, b: &RExpr, offset: usize) -> bool {
    use RExpr::*;
    match (a, b) {
        (Column(x), Column(y)) => *x == *y + offset,
        (ConstInt(x), ConstInt(y)) => x == y,
        (ConstBool(x), ConstBool(y)) => x == y,
        (ConstText(x), ConstText(y)) => x == y,
        (ConstNull, ConstNull) => true,
        (
            ScalarFunc {
                func: fa, args: aa, ..
            },
            ScalarFunc {
                func: fb, args: ab, ..
            },
        ) => {
            fa == fb
                && aa.len() == ab.len()
                && aa
                    .iter()
                    .zip(ab)
                    .all(|(x, y)| rexpr_eq_shifted(x, y, offset))
        }
        // COALESCE(a, b, …) is a legal (immutable-iff-args-are) index expression (grammar.md
        // §51), so an index on COALESCE(x, 0) must match the same spelling in a query.
        (
            Coalesce {
                args: aa,
                coerce_decimal: da,
            },
            Coalesce {
                args: ab,
                coerce_decimal: db,
            },
        ) => {
            da == db
                && aa.len() == ab.len()
                && aa
                    .iter()
                    .zip(ab)
                    .all(|(x, y)| rexpr_eq_shifted(x, y, offset))
        }
        // GREATEST/LEAST(a, b, …) is likewise a legal index expression (grammar.md §52); a
        // GREATEST index must not match a LEAST query (the `greatest` discriminant is compared),
        // nor an index built under a different text collation (compared by name — a collation-X
        // index must not answer a collation-Y query).
        (
            GreatestLeast {
                args: aa,
                coerce_decimal: da,
                greatest: ga,
                collation: ca,
            },
            GreatestLeast {
                args: ab,
                coerce_decimal: db,
                greatest: gb,
                collation: cb,
            },
        ) => {
            ga == gb
                && da == db
                && ca.as_ref().map(|c| &c.name) == cb.as_ref().map(|c| &c.name)
                && aa.len() == ab.len()
                && aa
                    .iter()
                    .zip(ab)
                    .all(|(x, y)| rexpr_eq_shifted(x, y, offset))
        }
        (
            Arith {
                op: oa,
                lhs: la,
                rhs: ra,
                ..
            },
            Arith {
                op: ob,
                lhs: lb,
                rhs: rb,
                ..
            },
        ) => oa == ob && rexpr_eq_shifted(la, lb, offset) && rexpr_eq_shifted(ra, rb, offset),
        (
            Cast {
                inner: ia,
                target: ta,
                typmod: None,
                varchar_len: None,
            },
            Cast {
                inner: ib,
                target: tb,
                typmod: None,
                varchar_len: None,
            },
        ) => ta == tb && rexpr_eq_shifted(ia, ib, offset),
        (Neg { operand: x, .. }, Neg { operand: y, .. }) => rexpr_eq_shifted(x, y, offset),
        (Not(x), Not(y)) => rexpr_eq_shifted(x, y, offset),
        // A comparison (`status = 'active'`, `amt > 0`) is the canonical partial-index predicate
        // shape (indexes.md §9): same operator + same derived collation + structurally-equal operands.
        (
            Compare {
                op: oa,
                lhs: la,
                rhs: ra,
                collation: ca,
            },
            Compare {
                op: ob,
                lhs: lb,
                rhs: rb,
                collation: cb,
            },
        ) => {
            oa == ob
                && ca.as_ref().map(|c| c.name.as_str()) == cb.as_ref().map(|c| c.name.as_str())
                && rexpr_eq_shifted(la, lb, offset)
                && rexpr_eq_shifted(ra, rb, offset)
        }
        (And(la, ra), And(lb, rb)) | (Or(la, ra), Or(lb, rb)) => {
            rexpr_eq_shifted(la, lb, offset) && rexpr_eq_shifted(ra, rb, offset)
        }
        (
            IsNull {
                operand: x,
                negated: na,
            },
            IsNull {
                operand: y,
                negated: nb,
            },
        ) => na == nb && rexpr_eq_shifted(x, y, offset),
        // `lower(x)` / `upper(x)` (spec/design/collation.md §16) resolve to a dedicated `Casing`
        // node — NOT `ScalarFunc` — so an index on `lower(email)` (the headline expression-index
        // shape) matches ONLY if this arm is present. The fold is deterministic (engine-global
        // casing regime, identical at index-build and query-eval), so the match is sound: same
        // `upper` direction + a matching argument.
        (Casing { upper: ua, arg: aa }, Casing { upper: ub, arg: ab }) => {
            ua == ub && rexpr_eq_shifted(aa, ab, offset)
        }
        _ => false,
    }
}

pub(crate) fn collect_bound_terms(
    e: &RExpr,
    key: &KeyMatch,
    pk_type: ScalarType,
    col_coll: Option<&str>,
    sibling_cutoff: Option<usize>,
    terms: &mut Vec<BoundTerm>,
) {
    match e {
        RExpr::And(l, r) => {
            collect_bound_terms(l, key, pk_type, col_coll, sibling_cutoff, terms);
            collect_bound_terms(r, key, pk_type, col_coll, sibling_cutoff, terms);
        }
        // `<>` is not a contiguous range, so it never seeds an index/PK bound — it stays in the
        // residual filter (a full scan + filter). Skipping it here keeps the deterministic cost
        // identical to Go/TS, where `asBoundTerm` excludes it the same way.
        // A comparison bounds the key only when ITS resolved collation matches the key column's
        // frozen collation (`col_coll`) — so the comparison orders text the SAME way the B-tree is
        // keyed (spec/design/collation.md §8). `C` key ⇔ a `C`/byte comparison (both `None`); a
        // collated key ⇔ a comparison under the SAME collation (the column's implicit collation, or
        // an explicit `COLLATE "<that name>"`). A comparison under a DIFFERENT collation —
        // `name COLLATE "C"` over a `unicode` column, `COLLATE "de"` over `unicode` — does NOT
        // match: its order disagrees with the stored keys, so it stays a full scan + residual
        // filter. (A *skewed* collated key never reaches here — `key_collation_ctx` refuses the
        // whole bound, §12.) The probe is then encoded in the key column's form (sort key for a
        // collated `Full` column — `build_key_bound`/`index_bound_rows`).
        RExpr::Compare {
            op,
            lhs,
            rhs,
            collation,
        } if !matches!(op, CmpOp::Ne)
            && collation.as_ref().map(|c| c.name.as_str()) == col_coll =>
        {
            let is_pk = |x: &RExpr| key.matches(x);
            // The key operand on either side (op flipped when it is on the right); the other side a
            // matching-type const-source. Anything else contributes no term.
            let term = if is_pk(lhs) {
                const_source(rhs, pk_type, sibling_cutoff).map(|src| BoundTerm { op: *op, src })
            } else if is_pk(rhs) {
                const_source(lhs, pk_type, sibling_cutoff).map(|src| BoundTerm {
                    op: flip_cmp(*op),
                    src,
                })
            } else {
                None
            };
            if let Some(t) = term {
                terms.push(t);
            }
        }
        _ => {}
    }
}

/// Recognize a const-source operand whose static type matches the PK's storage type (a promoted
/// comparison — e.g. `intpk = 2.5` → a `ConstDecimal` — does not match, so it stays residual). A
/// bare correlated `OuterColumn` IS a const-source (its value is a runtime constant for a given
/// outer row); arithmetic etc. are not. A type-mismatched outer reference is wrapped in a `Cast` by
/// the resolver (as for the literal case above), so it never arrives here bare — the type check is
/// implicit and the match stays sound.
///
/// `sibling_cutoff` opens the index-nested-loop door (cost.md §3 "JOIN"): when `Some(cut)`, a bare
/// `Column(g)` whose GLOBAL index is `< cut` — a column of an EARLIER join relation — is a
/// `BoundSrc::Sibling`, resolved per outer row from the combined left-hand row. Like `OuterColumn`,
/// a bare sibling column implies a type match (a mismatch is a `Cast`, never bare — sound). A
/// same-relation or later-relation column is `>= cut`, so it stays residual (`None`).
pub(crate) fn const_source(
    e: &RExpr,
    pk_type: ScalarType,
    sibling_cutoff: Option<usize>,
) -> Option<BoundSrc> {
    match e {
        RExpr::Param(i) => Some(BoundSrc::Param(*i)),
        RExpr::ConstNull => Some(BoundSrc::Null),
        RExpr::ConstInt(n) if pk_type.is_integer() => Some(BoundSrc::Int(*n)),
        RExpr::ConstBool(b) if pk_type.is_bool() => Some(BoundSrc::Bool(*b)),
        RExpr::ConstUuid(u) if pk_type.is_uuid() => Some(BoundSrc::Uuid(*u)),
        RExpr::ConstTimestamp(m) if pk_type.is_timestamp() => Some(BoundSrc::Timestamp(*m)),
        RExpr::ConstTimestamptz(m) if pk_type.is_timestamptz() => Some(BoundSrc::Timestamp(*m)),
        RExpr::ConstDate(d) if pk_type.is_date() => Some(BoundSrc::Date(*d)),
        RExpr::ConstText(s) if pk_type.is_text() => Some(BoundSrc::Text(s.clone())),
        RExpr::ConstBytea(b) if pk_type.is_bytea() => Some(BoundSrc::Bytea(b.clone())),
        RExpr::ConstDecimal(d) if pk_type.is_decimal() => Some(BoundSrc::Decimal(d.clone())),
        RExpr::ConstInterval(iv) if pk_type.is_interval() => Some(BoundSrc::Interval(*iv)),
        RExpr::OuterColumn { level, index } => Some(BoundSrc::Outer {
            level: *level,
            index: *index,
        }),
        RExpr::Column(g) if sibling_cutoff.is_some_and(|cut| *g < cut) => {
            Some(BoundSrc::Sibling(*g))
        }
        _ => None,
    }
}

/// Swap a comparison's sense (for `const <op> pk` ⇒ `pk <flipped> const`). Eq and Ne are symmetric.
pub(crate) fn flip_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
    }
}

/// Encode one source interval into logical key space. NULL makes the disjunct empty; an
/// unencodable equality cannot match and is also empty, while an unencodable range endpoint is
/// dropped as a sound widening because the complete predicate remains the residual filter.
fn build_logical_interval(
    key_type: ScalarType,
    terms: &[BoundTerm],
    params: &[Value],
    outer: &[&[Value]],
    coll: Option<&Collation>,
    left: &[Value],
) -> Option<KeyBound> {
    let mut b = KeyBound::unbounded();
    for term in terms {
        let key = match encode_bound_key(key_type, &term.src, params, outer, coll, left) {
            BoundKey::Null => return None,
            BoundKey::OutOfRange if matches!(term.op, CmpOp::Eq) => return None,
            BoundKey::OutOfRange => continue,
            BoundKey::Key(k) => k,
        };
        match term.op {
            CmpOp::Eq => {
                intersect_lo(&mut b, &key, true);
                intersect_hi(&mut b, &key, true);
            }
            CmpOp::Gt => intersect_lo(&mut b, &key, false),
            CmpOp::Ge => intersect_lo(&mut b, &key, true),
            CmpOp::Lt => intersect_hi(&mut b, &key, false),
            CmpOp::Le => intersect_hi(&mut b, &key, true),
            CmpOp::Ne => {}
        }
    }
    (!bound_empty(&b)).then_some(b)
}

fn intersect_bounds(mut a: KeyBound, b: &KeyBound) -> KeyBound {
    if let Some(lo) = &b.lo {
        intersect_lo(&mut a, lo, b.lo_inc);
    }
    if let Some(hi) = &b.hi {
        intersect_hi(&mut a, hi, b.hi_inc);
    }
    a
}

pub(crate) fn canonical_interval_set(
    key_type: ScalarType,
    specs: &[IntervalSpec],
    clip_terms: &[BoundTerm],
    params: &[Value],
    outer: &[&[Value]],
    coll: Option<&Collation>,
    left: &[Value],
) -> Vec<KeyBound> {
    let clip = if clip_terms.is_empty() {
        KeyBound::unbounded()
    } else if let Some(b) = build_logical_interval(key_type, clip_terms, params, outer, coll, left)
    {
        b
    } else {
        return Vec::new();
    };
    let mut intervals: Vec<KeyBound> = specs
        .iter()
        .filter_map(|spec| {
            let b = build_logical_interval(key_type, &spec.terms, params, outer, coll, left)?;
            let b = intersect_bounds(b, &clip);
            (!bound_empty(&b)).then_some(b)
        })
        .collect();
    intervals.sort_by(|a, b| match (&a.lo, &b.lo) {
        (None, None) => b.lo_inc.cmp(&a.lo_inc),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y).then_with(|| b.lo_inc.cmp(&a.lo_inc)),
    });
    let mut out: Vec<KeyBound> = Vec::with_capacity(intervals.len());
    for next in intervals {
        let Some(cur) = out.last_mut() else {
            out.push(next);
            continue;
        };
        let mut merge = cur.hi.is_none() || next.lo.is_none();
        if !merge {
            let cmp = next.lo.as_ref().unwrap().cmp(cur.hi.as_ref().unwrap());
            merge = cmp.is_lt() || (cmp.is_eq() && (cur.hi_inc || next.lo_inc));
            if !merge && key_type.is_fixed_width() && cur.hi_inc && next.lo_inc {
                merge = prefix_successor(cur.hi.as_ref().unwrap()) == next.lo;
            }
        }
        if !merge {
            out.push(next);
            continue;
        }
        if cur.hi.is_none() {
            continue;
        }
        match (&next.hi, &cur.hi) {
            (None, _) => {
                cur.hi = None;
                cur.hi_inc = false;
            }
            (Some(n), Some(c)) if n > c || (n == c && next.hi_inc) => {
                cur.hi = Some(n.clone());
                cur.hi_inc = next.hi_inc;
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn index_logical_interval(logical: &KeyBound) -> KeyBound {
    let mut b = KeyBound {
        lo: Some(vec![0x00]),
        lo_inc: true,
        hi: Some(vec![0x01]),
        hi_inc: false,
    };
    if let Some(lo) = &logical.lo {
        let mut p = vec![0x00];
        p.extend_from_slice(lo);
        if logical.lo_inc {
            intersect_lo(&mut b, &p, true);
        } else if let Some(next) = prefix_successor(&p) {
            intersect_lo(&mut b, &next, true);
        }
    }
    if let Some(hi) = &logical.hi {
        let mut p = vec![0x00];
        p.extend_from_slice(hi);
        if logical.hi_inc {
            if let Some(next) = prefix_successor(&p) {
                intersect_hi(&mut b, &next, false);
            }
        } else {
            intersect_hi(&mut b, &p, false);
        }
    }
    b
}

/// Build a PK tuple's concrete storage-key range. Equality members append bare encodings to P; a
/// complete tuple is `[P,P]`, a proper prefix is `[P,prefix-successor(P))`, and a next-member range
/// tightens that prefix interval.
pub(crate) fn build_key_bound(
    bp: &PkBound,
    params: &[Value],
    outer: &[&[Value]],
    left: &[Value],
) -> Option<KeyBound> {
    let mut p = Vec::new();
    for ec in &bp.eq_cols {
        let mut agreed: Option<Vec<u8>> = None;
        for src in &ec.srcs {
            let key =
                match encode_bound_key(ec.col_type, src, params, outer, ec.coll.as_deref(), left) {
                    BoundKey::Null => return None,
                    // Float bound encoding remains deferred. Preserve the old sound widening (and its
                    // INL re-scan shape), retaining only an already-encoded tuple prefix.
                    BoundKey::OutOfRange if ec.col_type.is_float() => {
                        return Some(if p.is_empty() {
                            KeyBound::unbounded()
                        } else {
                            KeyBound {
                                lo: Some(p.clone()),
                                lo_inc: true,
                                hi: prefix_successor(&p),
                                hi_inc: false,
                            }
                        });
                    }
                    BoundKey::OutOfRange => return None,
                    BoundKey::Key(k) => k,
                };
            match &agreed {
                None => agreed = Some(key),
                Some(prev) if *prev == key => {}
                Some(_) => return None,
            }
        }
        for term in &ec.ranges {
            let key = match encode_bound_key(
                ec.col_type,
                &term.src,
                params,
                outer,
                ec.coll.as_deref(),
                left,
            ) {
                BoundKey::Null => return None,
                BoundKey::OutOfRange => continue,
                BoundKey::Key(k) => k,
            };
            let cmp = agreed
                .as_ref()
                .expect("PK equality member has a source")
                .cmp(&key);
            let false_term = match term.op {
                CmpOp::Gt => !cmp.is_gt(),
                CmpOp::Ge => cmp.is_lt(),
                CmpOp::Lt => !cmp.is_lt(),
                CmpOp::Le => cmp.is_gt(),
                CmpOp::Eq | CmpOp::Ne => false,
            };
            if false_term {
                return None;
            }
        }
        p.extend_from_slice(&agreed.expect("PK equality member has a source"));
    }
    if bp.eq_cols.len() == bp.member_count {
        return Some(KeyBound {
            lo: Some(p.clone()),
            lo_inc: true,
            hi: Some(p),
            hi_inc: true,
        });
    }
    let mut b = KeyBound {
        lo: (!p.is_empty()).then(|| p.clone()),
        lo_inc: true,
        hi: prefix_successor(&p),
        hi_inc: false,
    };
    if let Some(rng) = &bp.range {
        for t in &rng.terms {
            let key = match encode_bound_key(
                rng.col_type,
                &t.src,
                params,
                outer,
                rng.coll.as_deref(),
                left,
            ) {
                BoundKey::Null => return None,
                BoundKey::OutOfRange => continue,
                BoundKey::Key(k) => k,
            };
            let mut endpoint = p.clone();
            endpoint.extend_from_slice(&key);
            match t.op {
                CmpOp::Gt => match prefix_successor(&endpoint) {
                    Some(next) => intersect_lo(&mut b, &next, true),
                    None => return None,
                },
                CmpOp::Ge => intersect_lo(&mut b, &endpoint, true),
                CmpOp::Lt => intersect_hi(&mut b, &endpoint, false),
                CmpOp::Le => {
                    if let Some(next) = prefix_successor(&endpoint) {
                        intersect_hi(&mut b, &next, false);
                    }
                }
                CmpOp::Eq | CmpOp::Ne => {}
            }
        }
    }
    if bound_empty(&b) { None } else { Some(b) }
}

/// Turn an index access predicate into a concrete index-key range at exec time (indexes.md §5.1).
/// Encode the equality prefix into `p` (the concatenated present slots), then — if there is a range
/// column — start from `[P, P‖0x01)` (the upper endpoint stops before the range column's NULL slot,
/// since a range is never true for NULL) and intersect each range term; otherwise the range is
/// `[P, byte-successor(P))` (every entry extending `P`). `None` ⇒ the bound admits no key (a NULL /
/// disagreeing prefix equality, a NULL range endpoint, or a contradictory range). The returned
/// `usize` is `len(P)`, the byte count the row-key suffix skip advances past the equality-prefix
/// slots before width-skipping the remaining components.
pub(crate) fn build_index_bound(
    ib: &IndexBound,
    params: &[Value],
    outer: &[&[Value]],
    left: &[Value],
) -> Option<(KeyBound, usize)> {
    let mut p: Vec<u8> = Vec::new();
    for ec in &ib.eq_cols {
        // Every equality const-source on this column must encode to ONE agreed value: a NULL is
        // 3VL-never-true, a disagreement (`a = 1 AND a = 2`) is a contradiction, and an out-of-range
        // integer can equal no stored value — all provably empty.
        let mut agreed: Option<Vec<u8>> = None;
        for src in &ec.srcs {
            let k =
                match encode_bound_key(ec.col_type, src, params, outer, ec.coll.as_deref(), left) {
                    BoundKey::Null | BoundKey::OutOfRange => return None,
                    BoundKey::Key(k) => k,
                };
            match &agreed {
                None => agreed = Some(k),
                Some(prev) if *prev == k => {}
                Some(_) => return None,
            }
        }
        p.push(0x00);
        p.extend_from_slice(&agreed.expect("an equality column has at least one source"));
    }
    let Some(rng) = &ib.range else {
        // Pure equality prefix: [P, byte-successor(P)).
        let b = KeyBound {
            lo: Some(p.clone()),
            lo_inc: true,
            hi: prefix_successor(&p),
            hi_inc: false,
        };
        return if bound_empty(&b) {
            None
        } else {
            Some((b, p.len()))
        };
    };
    // Equality prefix P + a range on the next column. Base: [P, P‖0x01) — present values only (the
    // 0x01 NULL tag sorts after every 0x00 present slot at this position).
    let mut hi_null = p.clone();
    hi_null.push(0x01);
    let mut b = KeyBound {
        lo: Some(p.clone()),
        lo_inc: true,
        hi: Some(hi_null),
        hi_inc: false,
    };
    for t in &rng.terms {
        // The range column is fixed-width (indexes.md §5.1 eligibility), so it is never collated: the
        // probe encodes with a `None` collation.
        let key = match encode_bound_key(rng.col_type, &t.src, params, outer, None, left) {
            BoundKey::Null => return None,
            BoundKey::OutOfRange => continue, // drop this half-bound (a wider, still-sound scan)
            BoundKey::Key(k) => k,
        };
        // P ‖ 0x00 ‖ encode(v) — the range column's present slot appended to the prefix.
        let mut ps = p.clone();
        ps.push(0x00);
        ps.extend_from_slice(&key);
        match t.op {
            CmpOp::Ge => intersect_lo(&mut b, &ps, true),
            // `>` skips the whole `c = v` subtree: the smallest key after every `P‖0x00‖v‖*` entry.
            CmpOp::Gt => match prefix_successor(&ps) {
                Some(s) => intersect_lo(&mut b, &s, true),
                None => return None, // no key exceeds the max — empty (unreachable: ps starts 0x00)
            },
            CmpOp::Lt => intersect_hi(&mut b, &ps, false),
            CmpOp::Le => match prefix_successor(&ps) {
                Some(s) => intersect_hi(&mut b, &s, false),
                None => {} // everything ≤ max — keep the base hi (P‖0x01)
            },
            // `=` never reaches range terms (filtered into the equality prefix); `<>` never becomes a
            // bound term at all. Both contribute no half-bound.
            CmpOp::Eq | CmpOp::Ne => {}
        }
    }
    if bound_empty(&b) {
        None
    } else {
        Some((b, p.len()))
    }
}

/// Encode a const-source's value into the PK's storage key (the same codec INSERT uses — `encode_int`
/// for integer/timestamp widths, the raw 16 bytes for uuid, the 1-byte `bool-byte` for boolean).
/// `Param`/`Outer`/`Sibling` resolve to a runtime `Value` first (the param table / the enclosing
/// outer row / the current combined left-hand row) and then encode through the shared path.
pub(crate) fn encode_bound_key(
    pk_ty: ScalarType,
    src: &BoundSrc,
    params: &[Value],
    outer: &[&[Value]],
    coll: Option<&Collation>,
    left: &[Value],
) -> BoundKey {
    match src {
        BoundSrc::Null => BoundKey::Null,
        BoundSrc::Int(n) => {
            if pk_ty.in_range(*n) {
                BoundKey::Key(encode_int(pk_ty, *n))
            } else {
                BoundKey::OutOfRange
            }
        }
        BoundSrc::Bool(b) => BoundKey::Key(encode_bool(*b)),
        BoundSrc::Uuid(u) => BoundKey::Key(u.to_vec()),
        BoundSrc::Timestamp(m) => BoundKey::Key(encode_int(pk_ty, *m)),
        BoundSrc::Date(d) => BoundKey::Key(encode_int(pk_ty, *d as i64)),
        BoundSrc::Text(s) => encode_text_bound(s, coll),
        BoundSrc::Bytea(b) => BoundKey::Key(encode_terminated(b)),
        BoundSrc::Decimal(d) => BoundKey::Key(d.encode_key()),
        BoundSrc::Interval(iv) => BoundKey::Key(iv.encode_key()),
        BoundSrc::Param(i) => encode_value_key(pk_ty, &params[*i], coll),
        // A correlated reference: column `index` of the enclosing row `level` hops out — the same
        // indexing the evaluator uses for `RExpr::OuterColumn` (innermost outer row is last).
        BoundSrc::Outer { level, index } => {
            encode_value_key(pk_ty, &outer[outer.len() - level][*index], coll)
        }
        // Index-nested-loop: the GLOBAL column index of an earlier join relation, read from the
        // current combined left-hand row (cost.md §3 "JOIN"). The join loop always passes a `left`
        // wide enough (the running row spans columns `[0, rel.offset)`, and `Sibling` indices are
        // `< rel.offset`); a stray out-of-range index widens to a full scan rather than panic.
        BoundSrc::Sibling(index) => match left.get(*index) {
            Some(v) => encode_value_key(pk_ty, v, coll),
            None => BoundKey::OutOfRange,
        },
    }
}

/// Encode a `text` probe into a key bound: the raw `text-terminated-escape` bytes for a `C` key
/// (`coll == None`, the fast path, encoding.md §2.4), or the collation's UCA sort key
/// (`text-collated-sortkey`, §2.12) for a `Full`-collated key. A sort-key build that fails on an
/// unmapped code point (the `0A000` the write/compare path raises, collation.md §6) becomes
/// `OutOfRange` here: the probe matches no stored (always-mapped) key, so the term contributes no
/// bound and the scan widens to a full scan + residual filter — which reproduces the exact
/// non-pushdown answer (empty for `=`, since equality is byte-identity §7; the `0A000` for an
/// ordering compare iff any row is actually scanned). Deterministic and identical across cores.
pub(crate) fn encode_text_bound(s: &str, coll: Option<&Collation>) -> BoundKey {
    match coll {
        Some(c) => match collation::sort_key(c, s) {
            Ok(k) => BoundKey::Key(k),
            Err(_) => BoundKey::OutOfRange,
        },
        None => BoundKey::Key(encode_terminated(s.as_bytes())),
    }
}

/// Encode a runtime `Value` (a bound param or a resolved outer column) into the PK's storage key.
/// A NULL value makes the comparison 3VL-unknown (an empty range); a value of a kind no key can
/// hold (or an integer outside the PK width) drops its half-bound, widening — still sound. `coll`
/// selects a `text` value's key form (collated sort key vs raw bytes — `encode_text_bound`).
pub(crate) fn encode_value_key(pk_ty: ScalarType, v: &Value, coll: Option<&Collation>) -> BoundKey {
    match v {
        Value::Null => BoundKey::Null,
        Value::Bool(b) => BoundKey::Key(encode_bool(*b)),
        Value::Uuid(u) => BoundKey::Key(u.to_vec()),
        Value::Int(n) => {
            if pk_ty.in_range(*n) {
                BoundKey::Key(encode_int(pk_ty, *n))
            } else {
                BoundKey::OutOfRange
            }
        }
        Value::Timestamp(m) | Value::Timestamptz(m) => BoundKey::Key(encode_int(pk_ty, *m)),
        Value::Date(d) => BoundKey::Key(encode_int(pk_ty, *d as i64)),
        Value::Text(s) => encode_text_bound(s, coll),
        Value::Bytea(b) => BoundKey::Key(encode_terminated(b)),
        Value::Decimal(d) => BoundKey::Key(d.encode_key()),
        Value::Interval(iv) => BoundKey::Key(iv.encode_key()),
        _ => BoundKey::OutOfRange,
    }
}

/// Tighten `b`'s lower bound to the more restrictive of (current, key); at an equal key an exclusive
/// bound (`inc=false`) wins.
pub(crate) fn intersect_lo(b: &mut KeyBound, key: &[u8], inc: bool) {
    let replace = match &b.lo {
        None => true,
        Some(cur) => key > cur.as_slice() || (key == cur.as_slice() && !inc),
    };
    if replace {
        b.lo = Some(key.to_vec());
        b.lo_inc = inc;
    }
}

/// Tighten `b`'s upper bound to the more restrictive of (current, key); at an equal key an exclusive
/// bound wins.
pub(crate) fn intersect_hi(b: &mut KeyBound, key: &[u8], inc: bool) {
    let replace = match &b.hi {
        None => true,
        Some(cur) => key < cur.as_slice() || (key == cur.as_slice() && !inc),
    };
    if replace {
        b.hi = Some(key.to_vec());
        b.hi_inc = inc;
    }
}

/// Whether the bound admits no key: lo above hi, or lo == hi with a non-inclusive endpoint.
pub(crate) fn bound_empty(b: &KeyBound) -> bool {
    match (&b.lo, &b.hi) {
        (Some(lo), Some(hi)) => {
            use std::cmp::Ordering::{Equal, Greater};
            match lo.cmp(hi) {
                Greater => true,
                Equal => !(b.lo_inc && b.hi_inc),
                _ => false,
            }
        }
        _ => false,
    }
}

/// A resolved set operation (spec/design/grammar.md §25): both operands planned with the same
/// parent scope (so a correlated set-op subquery works), the unified output types, and the
/// trailing ORDER BY / LIMIT / OFFSET resolved by output column.
pub(crate) struct SetOpPlan {
    pub(crate) op: SetOpKind,
    pub(crate) all: bool,
    pub(crate) lhs: QueryPlan,
    pub(crate) rhs: QueryPlan,
    pub(crate) column_names: Vec<String>,
    pub(crate) column_types: Vec<ResolvedType>,
    /// (output slot, descending, nulls_first) — the trailing ORDER BY resolved by output name.
    pub(crate) order: Vec<crate::spill::SortKey>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
}

/// A pull-based row cursor (Volcano-style): `next` yields one row, `None` at end of stream. The
/// cost meter is threaded IN per call rather than stored as a field, so the source holds no
/// borrow of it and the one `&mut Meter` is charged down a single call path with no aliasing —
/// the discipline that keeps this mirror-able with the Go/TS cores (CLAUDE.md §2). This is the
/// seam the streaming + point-lookup work (TODO Phase 6) builds on; today only `ScanSource`
/// exists and feeds the existing materialize-then-join pipeline unchanged, so results and cost
/// are byte-identical.
///
/// Charges the page_read block (one per B-tree node — spec/design/cost.md §3 "page_read") once,
/// before the first row, then storage_row_read per row yielded: the same units in the same order
/// as the inline scan loop it replaced. `rows` is the in-key-order materialization (eager today,
/// via `iter_in_key_order`; a lazy leaf walk later) — the charge accounting is identical either
/// way because cost is the logical node/row count, not a physical leaf fetch (pager.md §5). The
/// block fires on the first `next` even for an empty table (node_count 0 ⇒ a no-op charge), so
/// the accrued total never moves. `next` returns `Result` so the later lazy walk's leaf-fault
/// error has a home; the eager form never errors.
pub(crate) struct ScanSource {
    pub(crate) rows: std::vec::IntoIter<Row>,
    pub(crate) node_count: i64,
    pub(crate) charged_block: bool,
}

impl ScanSource {
    pub(crate) fn new(rows: Vec<Row>, node_count: i64) -> Self {
        ScanSource {
            rows: rows.into_iter(),
            node_count,
            charged_block: false,
        }
    }

    pub(crate) fn next(&mut self, m: &mut Meter) -> Result<Option<Row>> {
        // Enforce the cost ceiling before pulling the next row (CLAUDE.md §13): a runaway scan
        // (or a JOIN/correlated re-scan built on this source) stops deterministically once
        // accrued cost reaches the limit. No-op when unlimited (spec/design/cost.md §6).
        m.guard()?;
        if !self.charged_block {
            m.charge(COSTS.page_read * self.node_count);
            self.charged_block = true;
        }
        match self.rows.next() {
            Some(row) => {
                m.charge(COSTS.storage_row_read);
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }
}

// ============================================================================
// Aggregate resolution + accumulation (spec/design/aggregates.md).
//
// An aggregate query's select list resolves in `Collect` mode: each aggregate call is
// collected into an `AggSpec` (its plan + resolved argument) and replaced by a reference to
// a synthetic-row slot (an `RExpr::Column(slot)` indexing the finalized aggregate results),
// so the existing evaluator projects the result with no new node. Outside Collect mode
// (`Forbidden`: WHERE / ON / an aggregate's own argument / any non-aggregate query) a column
// resolves normally and an aggregate call is a 42803 grouping error.
// ============================================================================

/// The aggregate-resolution context threaded through `resolve`.
pub(crate) enum AggCtx {
    /// Aggregates are not allowed here (a FuncCall is 42803); columns resolve normally.
    Forbidden,
    /// An aggregate query's projection: a FuncCall collects into `specs` and resolves to a
    /// synthetic slot (group_keys.len() + its index); a column resolves to its position among
    /// `group_keys` (a synthetic slot in 0..group_keys.len()) if it is a grouping key, else
    /// 42803. `group_keys` holds the resolved flat indices of the GROUP BY columns (empty for
    /// whole-table aggregation — then every bare column is 42803). The synthetic row the
    /// projection evaluates against is `[group_key_values…, aggregate_results…]`.
    Collect {
        group_keys: Vec<usize>,
        /// Parallel to `group_keys`: for each master grouping key, `Some((canonical AST, type))` if
        /// it is a general **expression** key (`GROUP BY a + b`, aggregates.md §15) — so a projection
        /// / HAVING / ORDER BY expression that structurally matches it resolves to that group's
        /// synthetic slot — or `None` for a plain **column** key (matched by the column path instead).
        group_key_exprs: Vec<Option<(Expr, ResolvedType)>>,
        specs: Vec<AggSpec>,
        /// One entry per `GROUPING(c1, …, ck)` call collected from the projection / HAVING — each is
        /// the list of master-grouping-column POSITIONS (indices into `group_keys`) of its arguments.
        /// The call resolves to the placeholder slot `GROUPING_GS_BASE + index`, rebased after
        /// resolution to its real trailing synthetic slot (spec/design/aggregates.md §12).
        grouping_specs: Vec<Vec<usize>>,
    },
    /// A non-aggregate WINDOW query's projection (spec/design/window.md §5.1). Bare columns
    /// resolve to the real input row (like Forbidden); a `FuncCall` carrying an `OVER` clause
    /// collects into `specs` and resolves to the synthetic slot `base + window_index`, where
    /// A window function carrying an `OVER` clause collects into `specs` and resolves to the
    /// placeholder slot `WINDOW_RESULT_BASE + w` (rebased to `input_width + window_keys.len() + w`
    /// after resolution, once the row layout is final — like `GroupedWindow`). A non-column PARTITION
    /// BY / ORDER BY key (`PARTITION BY a + b`) is collected into `window_keys` and resolved to the
    /// placeholder slot `WINDOW_KEY_BASE + k`, rebased the same way.
    Window {
        specs: Vec<WindowSpec>,
        window_keys: Vec<RExpr>,
    },
    /// A GROUPED query that ALSO has window functions (spec/design/window.md §2/§5.1). The
    /// projection resolves against the grouped synthetic row `[group_keys…, agg_results…,
    /// window_results…]`: a bare column → its group-key slot (`42803` otherwise), a bare aggregate
    /// → an agg slot (`group_keys.len() + agg index`), and an `OVER` call → a window result. A
    /// window function's ARGUMENTS resolve under the grouped scope too (a nested aggregate collects
    /// into `agg_specs`, a bare column must be a grouping key), so `sum(sum(x)) OVER ()` is legal;
    /// its PARTITION BY / ORDER BY column keys must be grouping columns. Because the real window
    /// slot (`group_keys.len() + agg_specs.len() + w`) is not known until EVERY aggregate has been
    /// collected (one may be nested in a later window argument or the HAVING clause), a window
    /// result is resolved to the PLACEHOLDER slot `WINDOW_RESULT_BASE + w` and rewritten to its real
    /// slot by `rebase_placeholder_cols` after resolution finishes.
    GroupedWindow {
        group_keys: Vec<usize>,
        /// Parallel to `group_keys` — see `Collect::group_key_exprs` (general-expression group keys,
        /// aggregates.md §15). A grouped+window query matches them the same way in its projection.
        group_key_exprs: Vec<Option<(Expr, ResolvedType)>>,
        agg_specs: Vec<AggSpec>,
        /// `GROUPING(...)` calls collected from the projection / HAVING when the query ALSO has window
        /// functions (GROUPING SETS + window, aggregates.md §21) — same as `Collect::grouping_specs`.
        grouping_specs: Vec<Vec<usize>>,
        window_specs: Vec<WindowSpec>,
        /// Materialized window-key expressions (a non-column PARTITION BY / ORDER BY key —
        /// `PARTITION BY g + 1`, or `ORDER BY sum(x) + 1`), resolved against the grouped row and
        /// collected at the placeholder slot `WINDOW_KEY_BASE + k`. A bare grouping column or a bare
        /// aggregate (`ORDER BY sum(x)`) resolves to its real grouped-row slot and is NOT materialized.
        window_keys: Vec<RExpr>,
    },
}

/// The placeholder base a window query's window results carry until `rebase_placeholder_cols` rewrites
/// them to `input_width + window_keys.len() + w` (spec/design/window.md §5.1). Far above any real
/// column/synthetic-slot count, and below 2³¹ so it is valid on a 32-bit `usize` (the wasm32 build)
/// as well as f64-exact in the TS core's `number`. Kept identical across the three cores.
pub(crate) const WINDOW_RESULT_BASE: usize = 1 << 28;

/// The placeholder base a materialized window-key expression (a non-column PARTITION BY / ORDER BY
/// key — `PARTITION BY a + b`) carries until the rebase pass rewrites it to its real synthetic slot
/// `input_width + k` (spec/design/window.md §5.1). Disjoint from `WINDOW_RESULT_BASE`'s range, and
/// below 2³¹ (32-bit-`usize` / wasm32 safe). A bare-column key is NOT materialized — it keeps its real row slot.
pub(crate) const WINDOW_KEY_BASE: usize = 1 << 29;

/// The placeholder base a `GROUPING(...)` call carries until the rebase pass rewrites it to its real
/// trailing synthetic slot `group_keys.len() + agg_specs.len() + grouping_index` (the GROUPING
/// results follow the master columns + aggregate results in the grouped row —
/// spec/design/aggregates.md §12). Disjoint from the window bases, below 2³¹ (32-bit-`usize` / wasm32 safe).
/// GROUPING is mutually exclusive with window functions, so its placeholders never coexist with the
/// window ones in a projection.
pub(crate) const GROUPING_GS_BASE: usize = 1 << 30;

/// The placeholder base a materialized `ORDER BY` **expression** key's sort slot carries until it is
/// rebased to its real trailing slot `final_row_width + k` (the materialized order values are appended
/// after the input / window / grouped columns — grammar.md §10). Used only in the `SortKey` slot field
/// (a different namespace from the `RExpr::Column` bases above), but kept disjoint and below 2³¹
/// (32-bit-`usize` / wasm32 safe) for the same reasons. A column / ordinal key keeps its real slot.
pub(crate) const ORDER_EXPR_BASE: usize = 1 << 27;

/// The maximum number of grouping sets a `GROUP BY` may expand to (`CUBE` of n columns alone is
/// 2ⁿ). Beyond this the statement is aborted `54001` (statement_too_complex) — jed's structural-
/// complexity gate (a deliberate divergence from PostgreSQL's per-construct "CUBE is limited to 12
/// elements" / 54011; jed bounds the total expansion instead). spec/design/aggregates.md §12.
pub(crate) const MAX_GROUPING_SETS: usize = 4096;

/// One resolved window function (spec/design/window.md §5.1): its plan, the resolved PARTITION BY
/// key column slots (flat input-row indices), and the resolved within-partition ORDER BY (sort
/// keys over the input row, PK tie-break applied by the stable sort over the PK-ordered scan).
pub(crate) struct WindowSpec {
    pub(crate) plan: WindowPlan,
    pub(crate) partition: Vec<usize>,
    pub(crate) order: Vec<crate::spill::SortKey>,
    /// Resolved function arguments (empty for the no-argument ranking functions; `ntile`'s bucket
    /// count; lag/lead's value/offset/default; the aggregate operand; first/last/nth_value's value
    /// + nth_value's position).
    pub(crate) args: Vec<RExpr>,
    /// The resolved explicit frame; `None` is the default frame (RANGE UNBOUNDED PRECEDING TO
    /// CURRENT ROW with an ORDER BY, the whole partition without — window.md §6).
    pub(crate) frame: Option<ResolvedFrame>,
    /// `agg(x) FILTER (WHERE cond) OVER (…)` — a per-frame-row boolean restricting which frame rows
    /// fold into the window aggregate (aggregates.md §20). `Some` only for an aggregate window
    /// function (a non-aggregate window function with `FILTER` is `0A000`). A `FILTER` disables the
    /// sliding-frame optimization (a filtered row can't be cleanly un-folded) — every frame re-folds.
    pub(crate) filter: Option<RExpr>,
}

/// A resolved window frame (spec/design/window.md §6). `ROWS` physical offsets, `GROUPS` peer-group
/// offsets (both integer counts), and `RANGE` value offsets over the single ordering key.
pub(crate) struct ResolvedFrame {
    pub(crate) mode: crate::ast::FrameMode,
    pub(crate) start: ResolvedBound,
    pub(crate) end: ResolvedBound,
    /// Frame exclusion (`EXCLUDE …` — window.md §6): rows dropped from `[lo, hi)` per current row.
    pub(crate) exclude: crate::ast::FrameExclusion,
}

/// A resolved frame boundary. `Preceding`/`Following` carry the offset as a value: `Value::Int(n)`
/// (the row/group count) for `ROWS`/`GROUPS`, and the numeric `Value` (`Int` over an integer key,
/// `Decimal` over a decimal key) added to / subtracted from the ordering key for `RANGE`.
pub(crate) enum ResolvedBound {
    UnboundedPreceding,
    Preceding(Value),
    CurrentRow,
    Following(Value),
    UnboundedFollowing,
}

/// The runtime plan for one window function (spec/design/window.md §4). S0: `row_number` only;
/// ranking / offset / aggregate-window / frame plans land in S1–S4.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowPlan {
    /// `ROW_NUMBER()` — the 1-based sequence position within the partition (frame-insensitive).
    RowNumber,
    /// `RANK()` — 1 + the number of rows in earlier peer groups (ties share a rank, then a gap).
    Rank,
    /// `DENSE_RANK()` — 1 + the number of earlier peer groups (ties share a rank, no gap).
    DenseRank,
    /// `PERCENT_RANK()` — (rank − 1) / (N − 1), 0 when N = 1; decimal (divergence D2).
    PercentRank,
    /// `CUME_DIST()` — (# rows through the current peer group) / N; decimal (divergence D2).
    CumeDist,
    /// `NTILE(n)` — distribute the partition into n ranked buckets (larger first), numbered 1..n.
    /// Position-based (not peer-based); n ≤ 0 → 22014; NULL n → NULL for every row.
    Ntile,
    /// `LAG(v [,off [,def]])` / `LEAD(...)` — the value `off` positions back / forward in the
    /// partition; `def` (or NULL) when the offset leaves the partition. Frame-insensitive.
    Lag,
    Lead,
    /// An aggregate used as a window function (S3): `sum/count/min/max/avg(...) OVER (...)`, folded
    /// over the row's default frame (running with a window ORDER BY, whole-partition without) or an
    /// explicit frame (S4). Reuses the aggregate `Acc` kernels; the operand (if any) is `args[0]`.
    Agg(AggPlan),
    /// `FIRST_VALUE(v)` / `LAST_VALUE(v)` — the value of the frame's first / last row (S4). `args[0]`
    /// is the value expression; frame-sensitive.
    FirstValue,
    LastValue,
    /// `NTH_VALUE(v, n)` — the value of the frame's n-th row, NULL if the frame has < n rows (S4).
    /// `args[0]` is the value, `args[1]` the position; frame-sensitive.
    NthValue,
}

/// The runtime plan for one aggregate, fixed at resolve from the function + operand type
/// (the PG widening — spec/design/aggregates.md §3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggPlan {
    /// COUNT(*) — count every row (NULLs included).
    CountStar,
    /// COUNT(expr) — count non-NULL inputs.
    Count,
    /// SUM(i16|i32) — accumulate i64, result i64 (traps 22003 at the i64 bound).
    SumInt,
    /// SUM(i64|decimal) — accumulate decimal, result decimal (traps 22003 at the cap).
    SumDecimal,
    /// AVG — accumulate a decimal sum + i64 count; result sum/count (decimal), NULL if count 0.
    Avg,
    /// SUM(f32|f64) — the STREAMING scan-order running total (spec/design/float.md §7; fold order
    /// ledgered non-deterministic). Carries the width so the fold re-rounds at the input width.
    SumFloat(ScalarType),
    /// AVG(f32|f64) — SUM (streaming scan-order fold) / count, one final rounding at the input width.
    AvgFloat(ScalarType),
    Min,
    Max,
    /// json_agg / jsonb_agg (and the `_strict` variants) — aggregate the inputs' JSON images into a
    /// JSON array (json-sql-functions.md §4). `compact` selects the `json` (compact) vs `jsonb`
    /// (canonical) result render; `strict` skips a NULL input (else a NULL → JSON null).
    JsonAgg {
        compact: bool,
        strict: bool,
    },
    /// json_object_agg / jsonb_object_agg (and the `_unique` variants) — aggregate (key, value) pairs
    /// (a `Row` operand) into a JSON object (json-sql-functions.md §4). `json` selects the json
    /// (insertion order + dups + " : " spacing) vs jsonb (canonical, last-wins) render; `unique`
    /// errors `22030` on a duplicate key.
    JsonObjectAgg {
        json: bool,
        unique: bool,
    },
    /// `mode() WITHIN GROUP (ORDER BY x)` — the most frequent value (tie → first in sort order),
    /// result the input type (spec/design/aggregates.md §13). The direction + buffered values live
    /// on the `Acc`; this is just the kernel id (kept f64-free so AggPlan stays `Copy`/`Eq`).
    OrderedSetMode,
    /// `percentile_disc(f) WITHIN GROUP (ORDER BY x)` — the discrete percentile, an actual input
    /// value at row `ceil(f·N)`; result the input type. Direction + fraction live on the `Acc`.
    OrderedSetDisc,
    /// `percentile_cont(f) WITHIN GROUP (ORDER BY x)` — the continuous (interpolated) percentile;
    /// numeric input widened to f64, result f64. Direction + fraction live on the `Acc`.
    OrderedSetCont,
    /// `percentile_cont(f) WITHIN GROUP (ORDER BY x)` over an **interval** input — the continuous
    /// percentile interpolated in the interval domain (`lo + (hi-lo)·pct`, PG `interval_lerp`);
    /// result `interval` (spec/design/aggregates.md §13). Values buffered as `Value::Interval`.
    OrderedSetContInterval,
    /// `rank(args) WITHIN GROUP (ORDER BY keys)` — the **hypothetical-set** rank: 1 + the number of
    /// group rows that sort strictly before the hypothetical row `args` (result `i64`, §19).
    HypoRank,
    /// `dense_rank(args) WITHIN GROUP (ORDER BY keys)` — 1 + the number of DISTINCT group values that
    /// sort strictly before the hypothetical row (result `i64`, §19).
    HypoDenseRank,
    /// `percent_rank(args) WITHIN GROUP (ORDER BY keys)` — `(rank − 1) / N` (result `f64`, §19).
    HypoPercentRank,
    /// `cume_dist(args) WITHIN GROUP (ORDER BY keys)` — `(#rows ≤ hyp + 1) / (N + 1)` (`f64`, §19).
    HypoCumeDist,
}

/// The resolve-time parameters of an ordered-set aggregate (spec/design/aggregates.md §13), kept
/// off `AggPlan` (which is `Copy`/`Eq`). `desc` is the `WITHIN GROUP` sort direction; `frac` is the
/// resolved **direct argument** (the percentile fraction) — resolved in the grouped context so it
/// references grouping columns by their synthetic key slots (a non-grouped column is `42803`,
/// matching PG's *"direct arguments … must use only grouped columns"*) and is evaluated **per group**
/// at finalize against the synthetic row. `None` for `mode` (no direct argument).
pub(crate) struct OsaParams {
    pub(crate) desc: bool,
    pub(crate) frac: Option<RExpr>,
    /// The `WITHIN GROUP` key's collation — `Some` for an explicit `COLLATE` or a column's frozen
    /// non-`C` collation; `None` for the default byte (`C`) order (aggregates.md §13). The finalize
    /// sort applies it to the buffered text values.
    pub(crate) collation: Option<std::sync::Arc<Collation>>,
}

/// One resolved aggregate: its plan and its resolved argument expression (evaluated per
/// input row against the real row). `operand` is `None` for COUNT(*). `distinct` (`COUNT(DISTINCT
/// x)`, aggregates.md §5) folds only the distinct non-NULL argument values — the fold loop keeps a
/// per-group value-canonical set and skips a value already seen. `filter` (`SUM(x) FILTER (WHERE
/// cond)`, aggregates.md §11) is a resolved boolean predicate evaluated per input row; only rows
/// for which it is TRUE are folded (so the filter applies before the DISTINCT dedup). Both are
/// only set in the aggregation stage; a window aggregate is never DISTINCT or FILTERed (0A000,
/// rejected at resolve).
pub(crate) struct AggSpec {
    pub(crate) plan: AggPlan,
    pub(crate) operand: Option<RExpr>,
    pub(crate) distinct: bool,
    pub(crate) filter: Option<RExpr>,
    /// `Some` for an ordered-set aggregate (`mode`/`percentile_*` — aggregates.md §13): the
    /// `WITHIN GROUP` sort direction + the constant fraction. `None` for every ordinary aggregate.
    pub(crate) osa: Option<OsaParams>,
    /// `Some` for a hypothetical-set aggregate (`rank`/`dense_rank`/`percent_rank`/`cume_dist`
    /// `WITHIN GROUP` — aggregates.md §19): the hypothetical-row direct args + the `WITHIN GROUP`
    /// key operands + per-key sort specs. `None` otherwise. (`operand` is `None` here — the keys
    /// are buffered as a tuple per row from `hypo.keys`.)
    pub(crate) hypo: Option<HypoParams>,
}

/// A single `WITHIN GROUP` ordering-key sort spec (aggregates.md §13/§19): direction, NULL
/// placement, and optional collation (text keys only).
pub(crate) struct KeySort {
    pub(crate) desc: bool,
    pub(crate) nulls_first: bool,
    pub(crate) collation: Option<std::sync::Arc<Collation>>,
}

/// The resolve-time parameters of a hypothetical-set aggregate (aggregates.md §19). `args` are the
/// hypothetical-row direct arguments (evaluated **per group** at finalize, like an OSA fraction —
/// they may reference grouping columns); `keys` are the `WITHIN GROUP` key operands (evaluated **per
/// row** during the fold and buffered as a tuple); `sorts` is the per-key ordering spec. The three
/// vectors have equal length (the arity check at resolve).
pub(crate) struct HypoParams {
    pub(crate) args: Vec<RExpr>,
    pub(crate) keys: Vec<RExpr>,
    pub(crate) sorts: Vec<KeySort>,
}

/// A running aggregate accumulator (one per AggSpec), folded per input row then finalized.
/// `Clone` so the window stage can snapshot a running accumulator at each peer-group boundary
/// (a running aggregate window's default frame — spec/design/window.md §6) without consuming it.
#[derive(Clone)]
pub(crate) enum Acc {
    CountStar(i64),
    Count(i64),
    SumInt {
        sum: i64,
        seen: bool,
    },
    SumDecimal {
        sum: Decimal,
        seen: bool,
    },
    Avg {
        sum: Decimal,
        count: i64,
    },
    /// Float SUM/AVG: a STREAMING scan-order running total of the finite inputs (float.md §7), with
    /// NaN / ±Inf presence tracked so the special-value resolution stays order-independent. The fold
    /// ORDER is ledgered non-deterministic (determinism_exceptions.toml `float-sum-order`) — O(1)
    /// memory, no buffer/sort. `is_avg` selects the final SUM vs SUM/count; `width` re-rounds `total`
    /// to binary32 each add when f32. `count` is the non-NULL count.
    FloatFold {
        width: ScalarType,
        is_avg: bool,
        total: f64,
        count: i64,
        any_nan: bool,
        pos_inf: bool,
        neg_inf: bool,
    },
    MinMax {
        cur: Option<Value>,
        is_min: bool,
    },
    /// json_agg / jsonb_agg accumulator (B4): the inputs' JSON-image nodes in row order. `compact`
    /// selects the json vs jsonb finalize type; `strict` skips NULL inputs. `seen` records whether the
    /// group had ANY input row: a zero-row group → SQL NULL, but a non-empty group all of whose rows
    /// the strict filter dropped → an empty array `[]` (PG distinguishes the two).
    JsonAgg {
        nodes: Vec<JsonNode>,
        compact: bool,
        strict: bool,
        seen: bool,
    },
    /// json_object_agg / jsonb_object_agg accumulator (B4): the (key, value) pairs in row order.
    /// `json` selects the json vs jsonb finalize render; `unique` errors `22030` on a duplicate key.
    /// `seen` distinguishes a zero-row group (→ NULL) from a non-empty one (→ an object, maybe `{}`).
    JsonObjectAgg {
        pairs: Vec<(String, Value)>,
        json: bool,
        unique: bool,
        seen: bool,
    },
    /// An ordered-set aggregate (`mode`/`percentile_disc`/`percentile_cont` — aggregates.md §13):
    /// buffer every non-NULL operand value, then sort + compute at finalize. `kind` selects the
    /// computation, `desc` the `WITHIN GROUP` direction. `frac` is the **evaluated** percentile
    /// fraction for this group (the direct argument is evaluated per group against the synthetic row
    /// just before finalize — aggregates.md §13): `Some(Value)` for `percentile_*` (the value may be
    /// `Value::Null` → NULL result, or an array → one percentile per element), `None` for `mode`. For
    /// `percentile_cont` the inputs are widened to f64 into `floats`; `mode`/`percentile_disc` buffer
    /// the original `Value`s into `vals`.
    OrderedSet {
        kind: AggPlan,
        desc: bool,
        frac: Option<Value>,
        /// The `WITHIN GROUP` key collation (aggregates.md §13) applied to the finalize sort of the
        /// buffered text values; `None` is the default byte (`C`) order.
        collation: Option<std::sync::Arc<Collation>>,
        vals: Vec<Value>,
        floats: Vec<f64>,
    },
    /// A hypothetical-set aggregate (`rank`/`dense_rank`/`percent_rank`/`cume_dist` — aggregates.md
    /// §19): buffer every row's `WITHIN GROUP` key tuple; at finalize (in the group-emission loop,
    /// where the per-group hypothetical row + the spec's sort specs are available) count how that
    /// hypothetical row would rank. `kind` selects the result formula.
    Hypothetical {
        kind: AggPlan,
        rows: Vec<Vec<Value>>,
    },
}

/// Compute an ordered-set aggregate's value over its collected group (spec/design/aggregates.md
/// §13). `mode` returns the most frequent value (tie → first in `WITHIN GROUP` sort order);
/// `percentile_disc` an actual value at row `ceil(p·N)`; `percentile_cont` the interpolated f64.
/// The fraction range check (`22003`) fires here, after the NULL-fraction check and before the
/// empty-group check — matching PG.
pub(crate) fn finalize_ordered_set(
    kind: AggPlan,
    desc: bool,
    collation: Option<&Collation>,
    frac: Option<&Value>,
    mut vals: Vec<Value>,
    mut floats: Vec<f64>,
) -> Result<Value> {
    match kind {
        AggPlan::OrderedSetMode => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            // Sort by the WITHIN GROUP order (honoring the key's collation), then take the first
            // value of the longest run of equal values — the most frequent, ties broken by sort
            // order (the first such run). Run equality is value-canonical (byte equality), so the
            // collation affects only which tied value comes first.
            sort_osa_vals(&mut vals, collation, desc)?;
            let mut best_idx = 0usize;
            let mut best_count = 1usize;
            let mut run_start = 0usize;
            for i in 1..vals.len() {
                if value_cmp(&vals[i], &vals[run_start]) == std::cmp::Ordering::Equal {
                    let run_len = i - run_start + 1;
                    if run_len > best_count {
                        best_count = run_len;
                        best_idx = run_start;
                    }
                } else {
                    run_start = i;
                }
            }
            Ok(vals.swap_remove(best_idx))
        }
        AggPlan::OrderedSetDisc => {
            // percentile_disc: an actual sorted value at row ceil(p·N). The fraction may be a scalar
            // or an array (aggregates.md §18); `finalize_percentile` dispatches and applies the
            // NULL / range-check / empty rules per PG, computing each percentile over the sorted vals.
            sort_osa_vals(&mut vals, collation, desc)?;
            finalize_percentile(frac, vals.is_empty(), |p| Ok(percentile_disc_at(&vals, p)))
        }
        AggPlan::OrderedSetCont => {
            floats.sort_by(|a, b| dir_cmp(crate::value::total_cmp_f64(*a, *b), desc));
            finalize_percentile(frac, floats.is_empty(), |p| {
                Ok(Value::Float64(percentile_cont_at(&floats, p)))
            })
        }
        AggPlan::OrderedSetContInterval => {
            // percentile_cont over interval input: interpolate in the interval domain (PG
            // `interval_lerp` — aggregates.md §13). Values are sorted by their canonical span
            // (interval has no collation, so `sort_osa_vals` uses the value order).
            sort_osa_vals(&mut vals, collation, desc)?;
            finalize_percentile(frac, vals.is_empty(), |p| {
                let n = vals.len();
                let pos = p * ((n - 1) as f64);
                let first = pos.floor() as usize;
                let second = pos.ceil() as usize;
                let lo = expect_interval(&vals[first]);
                if first == second {
                    return Ok(Value::Interval(lo));
                }
                let hi = expect_interval(&vals[second]);
                Ok(Value::Interval(interval_lerp(lo, hi, pos - first as f64)?))
            })
        }
        _ => unreachable!("finalize_ordered_set is only called for the ordered-set plans"),
    }
}

/// Apply the percentile fraction (scalar or array) to a sorted group, computing each percentile via
/// `compute` (spec/design/aggregates.md §13/§18). PG's check order is preserved: a **scalar** NULL
/// fraction → NULL; otherwise the range check (`22003`) fires per fraction **before** the empty-group
/// check; an empty/all-NULL group → NULL (the whole result, even for an array). For an **array**
/// fraction the result is an array with one percentile per element (a NULL element → a NULL element),
/// after every non-NULL element has passed the range check.
pub(crate) fn finalize_percentile(
    frac: Option<&Value>,
    empty: bool,
    compute: impl Fn(f64) -> Result<Value>,
) -> Result<Value> {
    match frac {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::Array(arr)) => {
            // Range-check every non-NULL element FIRST (before the empty-group check, PG).
            let mut fracs: Vec<Option<f64>> = Vec::with_capacity(arr.elements.len());
            for el in &arr.elements {
                let pf = fraction_to_f64(Some(el))?;
                if let Some(p) = pf {
                    check_percentile_fraction(p)?;
                }
                fracs.push(pf);
            }
            if empty {
                return Ok(Value::Null); // an empty/all-NULL group → NULL (not an array of NULLs), PG
            }
            let mut out = Vec::with_capacity(fracs.len());
            for pf in fracs {
                out.push(match pf {
                    Some(p) => compute(p)?,
                    None => Value::Null,
                });
            }
            let n = out.len();
            Ok(Value::Array(crate::value::ArrayVal {
                dims: vec![n],
                lbounds: vec![1],
                elements: out,
            }))
        }
        Some(scalar) => {
            let Some(p) = fraction_to_f64(Some(scalar))? else {
                return Ok(Value::Null);
            };
            check_percentile_fraction(p)?;
            if empty {
                return Ok(Value::Null);
            }
            compute(p)
        }
    }
}

/// The `Interval` of a buffered `Value::Interval` (an `OrderedSetContInterval` group only ever
/// buffers intervals — the resolver gates the operand to `interval`).
pub(crate) fn expect_interval(v: &Value) -> crate::interval::Interval {
    match v {
        Value::Interval(iv) => *iv,
        other => unreachable!("percentile_cont(interval) buffered a non-interval: {other:?}"),
    }
}

/// `interval_lerp(lo, hi, pct)` = `lo + (hi - lo)·pct`, PG's `orderedsetaggs.c` interval
/// interpolation (spec/design/aggregates.md §13). `interval_mul` below replicates PG's exact
/// field-cascade + rounding so the result is byte-identical to PostgreSQL.
pub(crate) fn interval_lerp(
    lo: crate::interval::Interval,
    hi: crate::interval::Interval,
    pct: f64,
) -> Result<crate::interval::Interval> {
    let diff = hi.sub(&lo)?;
    let scaled = interval_mul(diff, pct)?;
    scaled.add(&lo)
}

/// `interval * f64`, byte-identical to PostgreSQL's `interval_mul` (timestamp.c): multiply each
/// field by the factor, then cascade the fractional month/day parts down to days/micros with PG's
/// `TSROUND` (round to microsecond precision) and the `30 days/month`, `86400 s/day` conversions.
/// The operand is finite (no infinite intervals here) and the factor is a finite fraction in [0,1].
pub(crate) fn interval_mul(
    span: crate::interval::Interval,
    factor: f64,
) -> Result<crate::interval::Interval> {
    const DAYS_PER_MONTH: f64 = 30.0;
    const SECS_PER_DAY: f64 = 86400.0;
    const USECS_PER_SEC: f64 = 1_000_000.0;
    // TSROUND: round to microsecond precision (PG TS_PREC_INV = 1e6). PG uses `rint` — round to
    // nearest, ties to EVEN — so the result is byte-identical to PostgreSQL (not half-away-from-zero).
    let tsround = |j: f64| -> f64 { (j * USECS_PER_SEC).round_ties_even() / USECS_PER_SEC };
    let oor = || EngineError::new(SqlState::DatetimeFieldOverflow, "interval out of range");
    let fits_i32 = |x: f64| x >= i32::MIN as f64 && x < -(i32::MIN as f64);
    let fits_i64 = |x: f64| x >= i64::MIN as f64 && x < -(i64::MIN as f64);

    let orig_month = span.months;
    let orig_day = span.days;

    let result_double = span.months as f64 * factor;
    if result_double.is_nan() || !fits_i32(result_double) {
        return Err(oor());
    }
    let result_month = result_double as i32;

    let result_double = span.days as f64 * factor;
    if result_double.is_nan() || !fits_i32(result_double) {
        return Err(oor());
    }
    let mut result_day = result_double as i32;

    // Cascade fractional months → days, fractional days → micros (PG's exact sequence).
    let month_remainder_days =
        tsround((orig_month as f64 * factor - result_month as f64) * DAYS_PER_MONTH);
    let mut sec_remainder = tsround(
        (orig_day as f64 * factor - result_day as f64 + month_remainder_days
            - month_remainder_days as i64 as f64)
            * SECS_PER_DAY,
    );
    // Might exceed a day from rounding / cascade — push whole days up.
    if sec_remainder.abs() >= SECS_PER_DAY {
        result_day = result_day
            .checked_add((sec_remainder / SECS_PER_DAY) as i32)
            .ok_or_else(oor)?;
        sec_remainder -= (sec_remainder / SECS_PER_DAY) as i64 as f64 * SECS_PER_DAY;
    }
    result_day = result_day
        .checked_add(month_remainder_days as i32)
        .ok_or_else(oor)?;
    let result_double =
        (span.micros as f64 * factor + sec_remainder * USECS_PER_SEC).round_ties_even();
    if result_double.is_nan() || !fits_i64(result_double) {
        return Err(oor());
    }
    Ok(crate::interval::Interval {
        months: result_month,
        days: result_day,
        micros: result_double as i64,
    })
}

/// Compute a hypothetical-set aggregate's value (aggregates.md §19): given the buffered group key
/// tuples `rows`, the per-group hypothetical row `hyp`, and the `WITHIN GROUP` per-key sort specs,
/// count where `hyp` would rank. `rank` = 1 + rows strictly before `hyp`; `dense_rank` = 1 + distinct
/// values strictly before; `percent_rank` = `(rank-1)/N`; `cume_dist` = `(#rows ≤ hyp + 1)/(N+1)` —
/// PG's `orderedsetaggs.c` formulas exactly. Over an empty group: rank/dense_rank 1, percent_rank 0,
/// cume_dist 1.
pub(crate) fn finalize_hypothetical(
    kind: AggPlan,
    rows: &[Vec<Value>],
    hyp: &[Value],
    sorts: &[KeySort],
) -> Result<Value> {
    use std::cmp::Ordering;
    let n = rows.len();
    if n == 0 {
        return Ok(match kind {
            AggPlan::HypoRank | AggPlan::HypoDenseRank => Value::Int(1),
            AggPlan::HypoPercentRank => Value::Float64(0.0),
            AggPlan::HypoCumeDist => Value::Float64(1.0),
            _ => unreachable!("finalize_hypothetical only for the hypothetical-set plans"),
        });
    }
    let mut strictly_before = 0i64;
    let mut le = 0i64; // rows that sort ≤ hyp (for cume_dist's rank with flag +1)
    // The distinct strictly-before key tuples (for dense_rank), value-canonical (the group-key Eq).
    let mut distinct: HashSet<&Vec<Value>> = HashSet::new();
    for r in rows {
        match hypo_cmp(r, hyp, sorts)? {
            Ordering::Less => {
                strictly_before += 1;
                le += 1;
                distinct.insert(r);
            }
            Ordering::Equal => le += 1,
            Ordering::Greater => {}
        }
    }
    Ok(match kind {
        AggPlan::HypoRank => Value::Int(strictly_before + 1),
        AggPlan::HypoDenseRank => Value::Int(distinct.len() as i64 + 1),
        AggPlan::HypoPercentRank => Value::Float64(strictly_before as f64 / n as f64),
        AggPlan::HypoCumeDist => Value::Float64((le + 1) as f64 / (n + 1) as f64),
        _ => unreachable!("finalize_hypothetical only for the hypothetical-set plans"),
    })
}

/// Compare a buffered key tuple `a` to the hypothetical row `b` by the `WITHIN GROUP` order
/// (aggregates.md §19): the first key whose comparison is non-equal decides. Each key honors its
/// NULL placement, direction, and collation (a collated text key can fail `0A000`).
pub(crate) fn hypo_cmp(a: &[Value], b: &[Value], sorts: &[KeySort]) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    for (i, ks) in sorts.iter().enumerate() {
        let ord = compare_hypo_key(&a[i], &b[i], ks)?;
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// Compare one `WITHIN GROUP` key pair under its sort spec (NULL placement + direction + collation),
/// mirroring `key_cmp` plus the collated-text path (aggregates.md §19).
pub(crate) fn compare_hypo_key(a: &Value, b: &Value, ks: &KeySort) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    Ok(match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if ks.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if ks.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        _ => {
            let base = match (&ks.collation, a, b) {
                (Some(c), Value::Text(x), Value::Text(y)) => collated_cmp(c, x, y)?,
                _ => value_cmp(a, b),
            };
            if ks.desc { base.reverse() } else { base }
        }
    })
}

/// Convert an evaluated percentile fraction (the direct argument, evaluated per group) to `f64`
/// (aggregates.md §13/§17). `None` / `Value::Null` → `None` (a NULL fraction yields NULL). A numeric
/// value (the resolver restricts the fraction to a numeric family) widens via the IEEE / correctly-
/// rounded decimal cast. The range check (`22003`) is applied by the caller after this.
pub(crate) fn fraction_to_f64(frac: Option<&Value>) -> Result<Option<f64>> {
    Ok(match frac {
        None | Some(Value::Null) => None,
        Some(Value::Float64(f)) => Some(*f),
        Some(Value::Float32(f)) => Some(*f as f64),
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Decimal(d)) => match decimal_to_float(d, ScalarType::Float64)? {
            Value::Float64(f) => Some(f),
            _ => unreachable!("decimal_to_float(_, Float64) yields a Float64"),
        },
        Some(other) => {
            unreachable!("a non-numeric percentile fraction is rejected at resolve: {other:?}")
        }
    })
}

/// `percentile_disc` over the already-sorted group values: the value at row `ceil(p·N)` (1-based),
/// i.e. the smallest `K` with `K/N ≥ p` (PG `orderedsetaggs.c`). Caller guarantees non-empty + the
/// fraction in range. Takes `&[Value]` (clones the picked value) so an array fraction can read it
/// repeatedly. spec/design/aggregates.md §13.
pub(crate) fn percentile_disc_at(vals: &[Value], p: f64) -> Value {
    let n = vals.len();
    let rownum = (p * n as f64).ceil() as i64;
    let idx = if rownum < 1 { 0 } else { (rownum - 1) as usize };
    let idx = idx.min(n - 1);
    vals[idx].clone()
}

/// `percentile_cont` over the already-sorted f64 group values: interpolate between the two bracketing
/// rows, in f64 with PG's exact operation order — bit-identical across cores and to PG
/// (spec/design/aggregates.md §13). Caller guarantees non-empty + the fraction in range.
pub(crate) fn percentile_cont_at(floats: &[f64], p: f64) -> f64 {
    let n = floats.len();
    let pos = p * ((n - 1) as f64);
    let first = pos.floor() as usize;
    let second = pos.ceil() as usize;
    if first == second {
        floats[first]
    } else {
        let lo = floats[first];
        let hi = floats[second];
        let proportion = pos - first as f64;
        lo + (proportion * (hi - lo))
    }
}

/// Apply a `WITHIN GROUP` sort direction to a comparison result (DESC reverses).
pub(crate) fn dir_cmp(ord: std::cmp::Ordering, desc: bool) -> std::cmp::Ordering {
    if desc { ord.reverse() } else { ord }
}

/// Sort an ordered-set aggregate's buffered values by its `WITHIN GROUP` order (aggregates.md §13).
/// With no collation, the value-canonical comparison (the same total order `ORDER BY`/`MIN`/`MAX`
/// use). With a collation, a stable decorate-sort by the precomputed collation `sort_key` bytes (a
/// collated key is always text; an unmapped code point fails `0A000` at this deterministic point,
/// like the query ORDER BY). The stable sort keeps collation-equal values in scan order, so the
/// result is deterministic and cross-core identical.
pub(crate) fn sort_osa_vals(
    vals: &mut Vec<Value>,
    collation: Option<&Collation>,
    desc: bool,
) -> Result<()> {
    match collation {
        None => {
            vals.sort_by(|a, b| dir_cmp(value_cmp(a, b), desc));
            Ok(())
        }
        Some(c) => {
            let mut decorated: Vec<(Vec<u8>, Value)> = Vec::with_capacity(vals.len());
            for v in vals.drain(..) {
                let key = match &v {
                    Value::Text(s) => collation::sort_key(c, s)?,
                    other => {
                        unreachable!("a collated WITHIN GROUP key buffers only text: {other:?}")
                    }
                };
                decorated.push((key, v));
            }
            decorated.sort_by(|a, b| dir_cmp(a.0.cmp(&b.0), desc));
            vals.extend(decorated.into_iter().map(|(_, v)| v));
            Ok(())
        }
    }
}

/// The percentile fraction range gate (spec/design/aggregates.md §13): `< 0`, `> 1`, or NaN is
/// `22003` (`numeric_value_out_of_range`), matching PG's "percentile value … is not between 0
/// and 1". Called per group at finalize, after the NULL-fraction check.
pub(crate) fn check_percentile_fraction(p: f64) -> Result<()> {
    if p.is_nan() || !(0.0..=1.0).contains(&p) {
        return Err(EngineError::new(
            SqlState::NumericValueOutOfRange,
            format!("percentile value {p} is not between 0 and 1"),
        ));
    }
    Ok(())
}

/// Widen a numeric value to f64 for `percentile_cont` (spec/design/aggregates.md §13): integers via
/// the IEEE cast, decimals via the correctly-rounded `decimal→f64` cast (matching PG's
/// `numeric→float8`), floats unchanged (f32 widened to its exact f64). The resolver restricts the
/// operand to a numeric family, so no other variant reaches here.
pub(crate) fn percentile_input_f64(v: &Value) -> Result<f64> {
    Ok(match v {
        Value::Int(i) => *i as f64,
        Value::Float32(f) => *f as f64,
        Value::Float64(f) => *f,
        Value::Decimal(d) => match decimal_to_float(d, ScalarType::Float64)? {
            Value::Float64(f) => f,
            _ => unreachable!("decimal_to_float(_, Float64) yields a Float64"),
        },
        _ => unreachable!("resolver restricts percentile_cont to a numeric operand"),
    })
}

/// Whether any select item contains an aggregate call — i.e. this is an aggregate query.
pub(crate) fn items_have_aggregate(items: &SelectItems) -> bool {
    match items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_aggregate(&it.expr)),
    }
}

/// Whether a window definition's PARTITION BY / ORDER BY keys contain an aggregate (`OVER (ORDER BY
/// sum(x))` — spec/design/window.md §5.1). Such an aggregate makes the query an aggregate query (a
/// whole-table aggregate if there is no GROUP BY), exactly as a top-level aggregate would, so the
/// window keys resolve against the grouped row. Used by both the inline-`over` walk in
/// `expr_has_aggregate` and the WINDOW-clause scan that computes `is_agg`.
pub(crate) fn window_def_has_aggregate(wd: &WindowDef) -> bool {
    wd.partition.iter().any(expr_has_aggregate)
        || wd.order.iter().any(|k| expr_has_aggregate(&k.expr))
}

/// Whether any WINDOW-clause entry's keys contain an aggregate (`WINDOW w AS (ORDER BY sum(x))`),
/// which — like a top-level aggregate — makes the query an aggregate query (spec/design/window.md
/// §5.1). The entries are still named references at this point (the OVER-name desugar runs later), so
/// the WINDOW clause is scanned directly.
pub(crate) fn windows_have_aggregate(windows: &[(String, WindowDef)]) -> bool {
    windows.iter().any(|(_, wd)| window_def_has_aggregate(wd))
}

/// The sub-expressions of one AST subscript spec (an index, or a slice's present bounds) — for the
/// `Expr` tree walkers.
pub(crate) fn subscript_spec_exprs(s: &SubscriptSpec) -> Vec<&Expr> {
    match s {
        SubscriptSpec::Index(i) => vec![i],
        SubscriptSpec::Slice(lo, hi) => lo.iter().chain(hi.iter()).collect(),
    }
}

/// Whether an expression tree contains an AGGREGATE call anywhere. A scalar-function call is
/// not itself an aggregate, but may CONTAIN one (`abs(sum(x))`), so its arguments are walked.
pub(crate) fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FuncCall {
            name,
            args,
            over,
            over_name,
            within_group,
            ..
        } => {
            // An aggregate name carrying OVER (inline or a named-window reference) is a WINDOW
            // function, not a bare aggregate (S3/S5, spec/design/window.md §5.1) — so it does not
            // make the query an aggregate query. (Detection runs before the OVER-name desugar.) But an
            // aggregate INSIDE its inline window definition's keys (`rank() OVER (ORDER BY sum(x))`)
            // does — those keys resolve against the grouped row (§5.1). A hypothetical-set name with a
            // WITHIN GROUP clause (`rank(x) WITHIN GROUP (…)`) is an aggregate (aggregates.md §19).
            (over.is_none() && over_name.is_none() && is_aggregate_name(name))
                || (within_group.is_some() && is_hypothetical_set_name(name))
                || args.iter().any(expr_has_aggregate)
                || over.as_deref().is_some_and(window_def_has_aggregate)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => expr_has_aggregate(inner),
        Expr::Collate { inner, .. } => expr_has_aggregate(inner),
        Expr::Unary { operand, .. } => expr_has_aggregate(operand),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_has_aggregate(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => expr_has_aggregate(ctx) || expr_has_aggregate(path),
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::In { lhs, list, .. } => {
            expr_has_aggregate(lhs) || list.iter().any(expr_has_aggregate)
        }
        Expr::Quantified { lhs, array, .. } => expr_has_aggregate(lhs) || expr_has_aggregate(array),
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(lo) || expr_has_aggregate(hi)
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().any(expr_has_aggregate),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_aggregate(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_has_aggregate(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(expr_has_aggregate)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(expr_has_aggregate)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_aggregate(c) || expr_has_aggregate(r))
                || els.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::Coalesce(args) => args.iter().any(expr_has_aggregate),
        Expr::GreatestLeast { args, .. } => args.iter().any(expr_has_aggregate),
        // A subquery is an independent query: an aggregate INSIDE it does not make the OUTER query
        // an aggregate query (the outer reference, if any, is just a constant to the subquery).
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// Whether any select item contains a window-function call (a `FuncCall` carrying `OVER`). A
/// window query resolves its projection in `AggCtx::Window` mode (spec/design/window.md §5.1).
pub(crate) fn items_have_window(items: &SelectItems) -> bool {
    match items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_window(&it.expr)),
    }
}

/// Whether any ORDER BY key is (or contains) a window function, so a query whose only `OVER` call
/// sits in the ORDER BY still sets up the window machinery (grammar.md §10, window.md §5.1). An
/// ordinal/column key carries no expression.
pub(crate) fn order_by_has_window(keys: &[OrderKey]) -> bool {
    keys.iter()
        .any(|k| k.expr.as_ref().is_some_and(expr_has_window))
}

/// Whether an expression tree contains a window-function call anywhere (a `FuncCall` whose `over`
/// is set). An ordinary call may CONTAIN one in its arguments (`abs(row_number() OVER ())`), so the
/// arguments are walked; a window call's own PARTITION BY / ORDER BY may not contain a window
/// function (that is rejected at resolve, 42P20), so they are not walked here.
pub(crate) fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::FuncCall {
            over,
            over_name,
            args,
            ..
        } => over.is_some() || over_name.is_some() || args.iter().any(expr_has_window),
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => expr_has_window(inner),
        Expr::Collate { inner, .. } => expr_has_window(inner),
        Expr::Unary { operand, .. } => expr_has_window(operand),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_has_window(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => expr_has_window(ctx) || expr_has_window(path),
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_window(lhs) || expr_has_window(rhs)
        }
        Expr::In { lhs, list, .. } => expr_has_window(lhs) || list.iter().any(expr_has_window),
        Expr::Quantified { lhs, array, .. } => expr_has_window(lhs) || expr_has_window(array),
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_window(lhs) || expr_has_window(lo) || expr_has_window(hi)
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            expr_has_window(lhs) || expr_has_window(rhs)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().any(expr_has_window),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_window(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_has_window(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(expr_has_window)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(expr_has_window)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_window(c) || expr_has_window(r))
                || els.as_deref().is_some_and(expr_has_window)
        }
        Expr::Coalesce(args) => args.iter().any(expr_has_window),
        Expr::GreatestLeast { args, .. } => args.iter().any(expr_has_window),
        // A subquery is an independent query: a window function inside it is the subquery's own.
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// Desugar `OVER name` references in a select list to their WINDOW-clause definitions before
/// resolution (spec/design/window.md §5): each window call carrying `over_name` gets the named
/// definition copied into `over`; an undefined name is 42704. After this every window call carries
/// an inline `over`, so resolution (S0–S4) handles named and inline windows uniformly.
pub(crate) fn desugar_items(
    items: &mut SelectItems,
    windows: &[(String, WindowDef)],
) -> Result<()> {
    if let SelectItems::Items(v) = items {
        for it in v.iter_mut() {
            desugar_named_windows(&mut it.expr, windows)?;
        }
    }
    Ok(())
}

/// Apply the base-window merge rules (spec/design/window.md §5, PostgreSQL
/// `transformWindowDefinitions`): a definition that names a base copies the base's `PARTITION BY`
/// and — if the base has one — its `ORDER BY`, and supplies its own frame. The extender may **not**
/// add a `PARTITION BY` (42P20, even when the base has none), may add an `ORDER BY` only when the
/// base has none (42P20 otherwise), and the base must **not** carry a frame (42P20). The three
/// checks fire in PostgreSQL's priority order: PARTITION, then ORDER, then frame. Returns the
/// merged inline definition (`base = None`).
pub(crate) fn extend_window(
    base: &WindowDef,
    ext: &WindowDef,
    base_name: &str,
) -> Result<WindowDef> {
    if !ext.partition.is_empty() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot override PARTITION BY clause of window \"{base_name}\""),
        ));
    }
    if !base.order.is_empty() && !ext.order.is_empty() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot override ORDER BY clause of window \"{base_name}\""),
        ));
    }
    if base.frame.is_some() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot copy window \"{base_name}\" because it has a frame clause"),
        ));
    }
    Ok(WindowDef {
        base: None,
        partition: base.partition.clone(),
        order: if base.order.is_empty() {
            ext.order.clone()
        } else {
            base.order.clone()
        },
        frame: ext.frame.clone(),
    })
}

/// Resolve a WINDOW clause into all-inline definitions (spec/design/window.md §5). Entries are
/// processed left-to-right; an entry naming a base extends an **already-resolved earlier** entry
/// (a self- or forward-reference is therefore "does not exist" — 42704), via `extend_window`. Every
/// entry is resolved — even ones no `OVER` references — matching PostgreSQL's whole-clause check.
pub(crate) fn resolve_window_clause(
    windows: &[(String, WindowDef)],
) -> Result<Vec<(String, WindowDef)>> {
    let mut resolved: Vec<(String, WindowDef)> = Vec::with_capacity(windows.len());
    for (name, def) in windows {
        let r = if let Some(base_name) = &def.base {
            let base = lookup_window(&resolved, base_name)?;
            extend_window(&base, def, base_name)?
        } else {
            def.clone()
        };
        resolved.push((name.clone(), r));
    }
    Ok(resolved)
}

/// Find a (resolved, `base = None`) window definition by name in `windows`, case-insensitively, or
/// raise 42704 `window "<name>" does not exist`. Returns an owned clone to avoid borrow conflicts.
pub(crate) fn lookup_window(windows: &[(String, WindowDef)], name: &str) -> Result<WindowDef> {
    windows
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, d)| d.clone())
        .ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedObject,
                format!("window \"{name}\" does not exist"),
            )
        })
}

pub(crate) fn desugar_named_windows(e: &mut Expr, windows: &[(String, WindowDef)]) -> Result<()> {
    match e {
        Expr::FuncCall {
            over,
            over_name,
            args,
            ..
        } => {
            if let Some(name) = over_name.take() {
                // `OVER name` — a pure reference: copy the named definition whole, frame included
                // (no merge rules; copying a framed window is only forbidden for the parenthesized
                // extend form below — window.md §5).
                let def = lookup_window(windows, &name)?;
                *over = Some(Box::new(def));
            } else if over.as_ref().is_some_and(|d| d.base.is_some()) {
                // `OVER (base …)` — an extend: merge the inline definition onto the named base.
                let d = over.as_deref_mut().expect("base implies over is Some");
                let base_name = d.base.take().expect("base.is_some() checked");
                let base = lookup_window(windows, &base_name)?;
                *d = extend_window(&base, d, &base_name)?;
            }
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        Expr::Cast { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Collate { inner, .. } => {
            desugar_named_windows(inner, windows)?;
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => {
            desugar_named_windows(operand, windows)?;
        }
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            desugar_named_windows(ctx, windows)?;
            desugar_named_windows(path, windows)?;
        }
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(rhs, windows)?;
        }
        Expr::In { lhs, list, .. } => {
            desugar_named_windows(lhs, windows)?;
            for x in list.iter_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::Quantified { lhs, array, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(array, windows)?;
        }
        Expr::Between { lhs, lo, hi, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(lo, windows)?;
            desugar_named_windows(hi, windows)?;
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(rhs, windows)?;
        }
        Expr::Row(items) | Expr::Array(items) => {
            for x in items.iter_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => {
            desugar_named_windows(base, windows)?;
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(o) = operand.as_deref_mut() {
                desugar_named_windows(o, windows)?;
            }
            for (c, r) in whens.iter_mut() {
                desugar_named_windows(c, windows)?;
                desugar_named_windows(r, windows)?;
            }
            if let Some(x) = els.as_deref_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::Coalesce(args) => {
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        Expr::GreatestLeast { args, .. } => {
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        // Leaves, subscripts, and subqueries (independent) carry no top-level window ref to rewrite.
        _ => {}
    }
    Ok(())
}

/// The structural CHECK-expression rejections (spec/design/constraints.md §4.1), applied in
/// a single depth-first pre-order walk before resolution: a subquery is 0A000, an aggregate
/// call 42803, a bind parameter 42P02 — PG's codes and messages (oracle-probed; PG
/// interleaves these with resolution in parse order, a documented micro-order divergence).
pub(crate) fn reject_check_structure(e: &Expr) -> Result<()> {
    match e {
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in check constraint",
        )),
        Expr::Param(n) => Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        )),
        Expr::FuncCall { name, args, .. } => {
            if is_aggregate_name(name) {
                return Err(EngineError::new(
                    SqlState::GroupingError,
                    "aggregate functions are not allowed in check constraints",
                ));
            }
            args.iter().try_for_each(reject_check_structure)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. } => Ok(()),
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => {
            reject_check_structure(inner)
        }
        Expr::Collate { inner, .. } => reject_check_structure(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => reject_check_structure(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            reject_check_structure(ctx)?;
            reject_check_structure(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(rhs)
        }
        Expr::In { lhs, list, .. } => {
            reject_check_structure(lhs)?;
            list.iter().try_for_each(reject_check_structure)
        }
        Expr::Quantified { lhs, array, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(array)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().try_for_each(reject_check_structure),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => reject_check_structure(base),
        // `t.*` cannot syntactically reach a CHECK expression (it is a select-item-only shape —
        // `CHECK (t.*)` is a 42601 in the parser); accept it structurally for exhaustiveness.
        Expr::QualifiedStar { .. } => Ok(()),
        Expr::Subscript { base, subscripts } => {
            reject_check_structure(base)?;
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .try_for_each(reject_check_structure)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(lo)?;
            reject_check_structure(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(op) = operand {
                reject_check_structure(op)?;
            }
            for (c, r) in whens {
                reject_check_structure(c)?;
                reject_check_structure(r)?;
            }
            match els {
                Some(e) => reject_check_structure(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args) => {
            for a in args {
                reject_check_structure(a)?;
            }
            Ok(())
        }
        Expr::GreatestLeast { args, .. } => {
            for a in args {
                reject_check_structure(a)?;
            }
            Ok(())
        }
    }
}

/// The structural rejections for a `DEFAULT` expression (constraints.md §2), a single
/// depth-first pre-walk run before name/type resolution (the same micro-order divergence from
/// PG that `reject_check_structure` carries). A default extends the CHECK rejections with one
/// more: it may **not reference a column** (it is computed before the row exists). Codes match
/// PostgreSQL (oracle-probed): a column reference / subquery is `0A000`, an aggregate `42803`,
/// a bind parameter `42P02`.
pub(crate) fn reject_default_structure(e: &Expr) -> Result<()> {
    match e {
        Expr::Column(_) | Expr::QualifiedColumn { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use column reference in DEFAULT expression",
        )),
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in DEFAULT expression",
        )),
        Expr::Param(n) => Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        )),
        Expr::FuncCall { name, args, .. } => {
            if is_aggregate_name(name) {
                return Err(EngineError::new(
                    SqlState::GroupingError,
                    "aggregate functions are not allowed in DEFAULT expressions",
                ));
            }
            args.iter().try_for_each(reject_default_structure)
        }
        Expr::Literal(_) | Expr::TypedLiteral { .. } => Ok(()),
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => {
            reject_default_structure(inner)
        }
        Expr::Collate { inner, .. } => reject_default_structure(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => reject_default_structure(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            reject_default_structure(ctx)?;
            reject_default_structure(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(rhs)
        }
        Expr::In { lhs, list, .. } => {
            reject_default_structure(lhs)?;
            list.iter().try_for_each(reject_default_structure)
        }
        Expr::Quantified { lhs, array, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(array)
        }
        Expr::Row(items) | Expr::Array(items) => {
            items.iter().try_for_each(reject_default_structure)
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => reject_default_structure(base),
        // `t.*` cannot syntactically reach a DEFAULT expression (select-item-only); accept
        // structurally for exhaustiveness.
        Expr::QualifiedStar { .. } => Ok(()),
        Expr::Subscript { base, subscripts } => {
            reject_default_structure(base)?;
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .try_for_each(reject_default_structure)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(lo)?;
            reject_default_structure(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(op) = operand {
                reject_default_structure(op)?;
            }
            for (c, r) in whens {
                reject_default_structure(c)?;
                reject_default_structure(r)?;
            }
            match els {
                Some(e) => reject_default_structure(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args) => {
            for a in args {
                reject_default_structure(a)?;
            }
            Ok(())
        }
        Expr::GreatestLeast { args, .. } => {
            for a in args {
                reject_default_structure(a)?;
            }
            Ok(())
        }
    }
}

/// The distinct columns a CHECK expression references, as indices into `columns` — the input
/// to PG's auto-naming rule (constraints.md §4.3: exactly one distinct column →
/// `<table>_<col>_check`). Resolution already validated every reference, so an unknown name
/// is simply skipped; a qualified reference counts its column like a bare one (oracle-probed).
pub(crate) fn check_referenced_columns(e: &Expr, columns: &[Column]) -> Vec<usize> {
    pub(crate) fn walk(e: &Expr, columns: &[Column], out: &mut Vec<usize>) {
        let mut note = |name: &str| {
            if let Some(i) = columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
            {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        };
        match e {
            Expr::Column(name) | Expr::QualifiedColumn { name, .. } => note(name),
            Expr::Literal(_) | Expr::TypedLiteral { .. } | Expr::Param(_) => {}
            Expr::Cast { inner, .. }
            | Expr::Collate { inner, .. }
            | Expr::Extract { source: inner, .. } => walk(inner, columns, out),
            Expr::Unary { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::IsJson { operand, .. }
            | Expr::JsonCtor { operand, .. } => walk(operand, columns, out),
            Expr::JsonExists { ctx, path, .. }
            | Expr::JsonValue { ctx, path, .. }
            | Expr::JsonQuery { ctx, path, .. } => {
                walk(ctx, columns, out);
                walk(path, columns, out);
            }
            Expr::Binary { lhs, rhs, .. }
            | Expr::IsDistinctFrom { lhs, rhs, .. }
            | Expr::Like { lhs, rhs, .. }
            | Expr::Regex { lhs, rhs, .. } => {
                walk(lhs, columns, out);
                walk(rhs, columns, out);
            }
            Expr::In { lhs, list, .. } => {
                walk(lhs, columns, out);
                for x in list {
                    walk(x, columns, out);
                }
            }
            Expr::Quantified { lhs, array, .. } => {
                walk(lhs, columns, out);
                walk(array, columns, out);
            }
            Expr::Between { lhs, lo, hi, .. } => {
                walk(lhs, columns, out);
                walk(lo, columns, out);
                walk(hi, columns, out);
            }
            Expr::Case {
                operand,
                whens,
                els,
            } => {
                if let Some(op) = operand {
                    walk(op, columns, out);
                }
                for (c, r) in whens {
                    walk(c, columns, out);
                    walk(r, columns, out);
                }
                if let Some(e) = els {
                    walk(e, columns, out);
                }
            }
            Expr::Coalesce(args) => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::GreatestLeast { args, .. } => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::FuncCall { args, .. } => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::Row(items) | Expr::Array(items) => {
                for it in items {
                    walk(it, columns, out);
                }
            }
            Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => walk(base, columns, out),
            // `t.*` cannot appear in a CHECK expression (select-item-only); no columns to note.
            Expr::QualifiedStar { .. } => {}
            Expr::Subscript { base, subscripts } => {
                walk(base, columns, out);
                for e in subscripts.iter().flat_map(subscript_spec_exprs) {
                    walk(e, columns, out);
                }
            }
            // Unreachable in a validated check (rejected by `reject_check_structure`).
            Expr::ScalarSubquery(_)
            | Expr::Exists(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. } => {}
        }
    }
    let mut out = Vec::new();
    walk(e, columns, &mut out);
    out
}

/// The structural rejections for a PARTIAL-index predicate (spec/design/indexes.md §9), applied
/// before resolution: a **subquery** is `0A000` (`cannot use subquery in index predicate`) and a
/// **bind parameter** `$N` is `42P02` (`there is no parameter $N`) — both admitted by the ordinary
/// resolver, so they are caught here (the aggregate `42803` / window `42P20` / non-boolean `42804`
/// rejections then fall out of the `Forbidden`-context boolean resolve). Reuses
/// [`index_expr_has_subquery`] for the subquery walk, then finds the first param.
pub(crate) fn reject_index_predicate_structure(e: &Expr) -> Result<()> {
    if index_expr_has_subquery(e) {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in index predicate",
        ));
    }
    if let Some(n) = index_expr_first_param(e) {
        return Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        ));
    }
    Ok(())
}

/// The 1-based index of the first bind parameter `$N` in an expression, or `None` if it has none
/// (used by [`reject_index_predicate_structure`]). A depth-first pre-order walk mirroring
/// [`index_expr_has_subquery`]'s traversal.
pub(crate) fn index_expr_first_param(e: &Expr) -> Option<u32> {
    match e {
        Expr::Param(n) => Some(*n),
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::QualifiedStar { .. } => None,
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => None,
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::IsNull { operand: inner, .. }
        | Expr::IsJson { operand: inner, .. }
        | Expr::JsonCtor { operand: inner, .. }
        | Expr::FieldAccess { base: inner, .. }
        | Expr::FieldStar { base: inner } => index_expr_first_param(inner),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            index_expr_first_param(ctx).or_else(|| index_expr_first_param(path))
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            index_expr_first_param(lhs).or_else(|| index_expr_first_param(rhs))
        }
        Expr::In { lhs, list, .. } => {
            index_expr_first_param(lhs).or_else(|| list.iter().find_map(index_expr_first_param))
        }
        Expr::Quantified { lhs, array, .. } => {
            index_expr_first_param(lhs).or_else(|| index_expr_first_param(array))
        }
        Expr::Between { lhs, lo, hi, .. } => index_expr_first_param(lhs)
            .or_else(|| index_expr_first_param(lo))
            .or_else(|| index_expr_first_param(hi)),
        Expr::Case {
            operand,
            whens,
            els,
        } => operand
            .as_deref()
            .and_then(index_expr_first_param)
            .or_else(|| {
                whens.iter().find_map(|(c, r)| {
                    index_expr_first_param(c).or_else(|| index_expr_first_param(r))
                })
            })
            .or_else(|| els.as_deref().and_then(index_expr_first_param)),
        Expr::Coalesce(args) => args.iter().find_map(index_expr_first_param),
        Expr::GreatestLeast { args, .. } => args.iter().find_map(index_expr_first_param),
        Expr::FuncCall { args, .. } => args.iter().find_map(index_expr_first_param),
        Expr::Row(items) | Expr::Array(items) => items.iter().find_map(index_expr_first_param),
        Expr::Subscript { base, subscripts } => index_expr_first_param(base).or_else(|| {
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .find_map(index_expr_first_param)
        }),
    }
}

/// Whether an index-key expression contains a SUBQUERY (spec/design/indexes.md §2): a scalar
/// subquery, `EXISTS`, `IN (subquery)`, or a quantified subquery. A subquery reads other rows, so
/// it is not a deterministic function of this row — `0A000` at CREATE INDEX (PostgreSQL: "cannot
/// use subquery in index expression"). Unlike an aggregate/window (rejected by resolution), the
/// resolver admits an uncorrelated subquery, so it is caught here.
pub(crate) fn index_expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => true,
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_)
        | Expr::QualifiedStar { .. } => false,
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::IsNull { operand: inner, .. }
        | Expr::IsJson { operand: inner, .. }
        | Expr::JsonCtor { operand: inner, .. }
        | Expr::FieldAccess { base: inner, .. }
        | Expr::FieldStar { base: inner } => index_expr_has_subquery(inner),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            index_expr_has_subquery(ctx) || index_expr_has_subquery(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            index_expr_has_subquery(lhs) || index_expr_has_subquery(rhs)
        }
        Expr::In { lhs, list, .. } => {
            index_expr_has_subquery(lhs) || list.iter().any(index_expr_has_subquery)
        }
        Expr::Quantified { lhs, array, .. } => {
            index_expr_has_subquery(lhs) || index_expr_has_subquery(array)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            index_expr_has_subquery(lhs)
                || index_expr_has_subquery(lo)
                || index_expr_has_subquery(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(index_expr_has_subquery)
                || whens
                    .iter()
                    .any(|(c, r)| index_expr_has_subquery(c) || index_expr_has_subquery(r))
                || els.as_deref().is_some_and(index_expr_has_subquery)
        }
        Expr::Coalesce(args) => args.iter().any(index_expr_has_subquery),
        Expr::GreatestLeast { args, .. } => args.iter().any(index_expr_has_subquery),
        Expr::FuncCall { args, .. } => args.iter().any(index_expr_has_subquery),
        Expr::Row(items) | Expr::Array(items) => items.iter().any(index_expr_has_subquery),
        Expr::Subscript { base, subscripts } => {
            index_expr_has_subquery(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(index_expr_has_subquery)
        }
    }
}

/// The auto-name part for one index key element (spec/design/indexes.md §2, PG's
/// `ChooseIndexColumnNames`): a column key contributes its (lowercased) column name; a
/// bare-function-call expression its function name (`lower(email)` → `lower`); any other
/// expression the literal `expr`.
pub(crate) fn index_name_part(elem: &crate::ast::IndexKeyElem) -> String {
    use crate::ast::IndexKeyElem;
    match elem {
        IndexKeyElem::Column(name) => name.to_ascii_lowercase(),
        IndexKeyElem::Expr {
            expr: Expr::FuncCall { name, .. },
            ..
        } => name.to_ascii_lowercase(),
        IndexKeyElem::Expr { .. } => "expr".to_string(),
    }
}

/// Whether an index-key expression calls a **non-immutable** built-in (spec/design/indexes.md §2):
/// the entropy/clock seam (`uuidv4`/`uuidv7`/`now`/`clock_timestamp` — `current_timestamp` desugars
/// to `now` — and `current_date`, the bare keyword's own catalog function) or the sequence
/// functions (`nextval`/`currval`/`setval`/`lastval`) / `current_setting`.
/// Such a function would let the index drift from the table, so it is `42P17` at CREATE INDEX. The
/// walk mirrors [`check_referenced_columns`] (subqueries are already rejected by resolution). The
/// session-timezone hazard (an expression over `timestamptz`) is handled separately by the caller
/// (a referenced-`timestamptz`-column / `timestamptz`-result check), so this covers only calls.
pub(crate) fn index_expr_nonimmutable_call(e: &Expr) -> bool {
    fn is_nonimmutable(name: &str) -> bool {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "uuidv4"
                | "uuidv7"
                | "now"
                | "clock_timestamp"
                | "current_date"
                | "nextval"
                | "currval"
                | "setval"
                | "lastval"
                | "current_setting"
        )
    }
    match e {
        Expr::FuncCall { name, args, .. } => {
            is_nonimmutable(name) || args.iter().any(index_expr_nonimmutable_call)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::IsNull { operand: inner, .. }
        | Expr::IsJson { operand: inner, .. }
        | Expr::JsonCtor { operand: inner, .. } => index_expr_nonimmutable_call(inner),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            index_expr_nonimmutable_call(ctx) || index_expr_nonimmutable_call(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            index_expr_nonimmutable_call(lhs) || index_expr_nonimmutable_call(rhs)
        }
        Expr::In { lhs, list, .. } => {
            index_expr_nonimmutable_call(lhs) || list.iter().any(index_expr_nonimmutable_call)
        }
        Expr::Quantified { lhs, array, .. } => {
            index_expr_nonimmutable_call(lhs) || index_expr_nonimmutable_call(array)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            index_expr_nonimmutable_call(lhs)
                || index_expr_nonimmutable_call(lo)
                || index_expr_nonimmutable_call(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(index_expr_nonimmutable_call)
                || whens.iter().any(|(c, r)| {
                    index_expr_nonimmutable_call(c) || index_expr_nonimmutable_call(r)
                })
                || els.as_deref().is_some_and(index_expr_nonimmutable_call)
        }
        // COALESCE is a pure combinator — immutable iff its arguments are (grammar.md §51).
        Expr::Coalesce(args) => args.iter().any(index_expr_nonimmutable_call),
        // GREATEST/LEAST is likewise a pure combinator — immutable iff its arguments are (§52).
        Expr::GreatestLeast { args, .. } => args.iter().any(index_expr_nonimmutable_call),
        Expr::Row(items) | Expr::Array(items) => items.iter().any(index_expr_nonimmutable_call),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => {
            index_expr_nonimmutable_call(base)
        }
        Expr::Subscript { base, subscripts } => {
            index_expr_nonimmutable_call(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(index_expr_nonimmutable_call)
        }
        // Rejected by resolution before this walk (0A000 subquery / select-item-only star).
        Expr::QualifiedStar { .. }
        | Expr::ScalarSubquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// The environment threaded into the per-row evaluator (spec/design/grammar.md §26): the
/// engine (to run a correlated subquery's plan), the bound parameters, and the stack of
/// enclosing rows (innermost LAST) a correlated reference reads. `outer` is empty at the top
/// level; a correlated subquery pushes the current row before running its inner plan, so an
/// `OuterColumn { level, index }` reads `outer[outer.len() - level][index]`.
pub(crate) struct EvalEnv<'a> {
    pub(crate) exec: &'a Engine,
    pub(crate) params: &'a [Value],
    pub(crate) outer: &'a [&'a [Value]],
    /// The per-statement entropy+clock state (spec/design/entropy.md §5): the uuidv7 monotonic
    /// counter + the once-resolved statement clock, behind a `Cell` (interior mutability — `EvalEnv`
    /// is `&`-shared; the draw order is fixed by eval order). The injected random/clock functions
    /// live on `exec.seam` (handle-scoped); only the volatile uuid generators touch any of this.
    pub(crate) rng: &'a std::cell::Cell<crate::seam::StmtRng>,
    /// The statement's CTE execution context (spec/design/cte.md §5), so a FROM reference at any
    /// nesting depth delivers a CTE's rows. `CteCtx::empty()` for every non-`WITH` statement.
    pub(crate) ctes: CteCtx<'a>,
}

/// Whether `plan` is the single-table, no-blocking-operator **streaming scan** shape
/// (spec/design/cost.md §3, streaming.md §4) — a single relation, no join / aggregate / window, an
/// output order the chosen bound already yields, and a real table store. Without ORDER BY, LIMIT
/// observes the access path's existing deterministic order. With ORDER BY, a PK/PK-set bound must
/// preserve PK order, or an ordered-index bound/set must walk the exact ordering index.
pub(crate) fn streaming_scan_eligible(plan: &SelectPlan) -> bool {
    if !(plan.rels.len() == 1
        && plan.joins.is_empty()
        && !plan.is_agg
        && !plan.has_window
        && plan.rels[0].srf.is_none()
        && plan.rels[0].cte.is_none()
        && plan.rels[0].derived.is_none())
    {
        return false;
    }
    let bound = plan.phys.rel_bounds[0].as_ref();
    if plan.order.is_empty() {
        return !plan.distinct && plan.limit.is_some();
    }
    if plan.phys.pk_ordered {
        return matches!(
            bound,
            None | Some(ScanBound::Pk(_))
                | Some(ScanBound::PkSet(_))
                | Some(ScanBound::Gin(_))
                | Some(ScanBound::Gist(_))
        );
    }
    match (&plan.phys.index_order, bound) {
        (Some(io), Some(bound)) => index_order_compatible_bound(io, Some(bound)),
        _ => false,
    }
}

pub(crate) fn pull_streaming_scan_eligible(plan: &SelectPlan) -> bool {
    streaming_scan_eligible(plan)
        && matches!(plan.phys.rel_bounds[0], None | Some(ScanBound::Pk(_)))
}

pub(crate) fn index_order_compatible_bound(io: &IndexOrder, bound: Option<&ScanBound>) -> bool {
    match bound {
        None => true,
        Some(ScanBound::Index(ib)) => ib.name_key == io.name_key,
        Some(ScanBound::IndexSet(ks)) => ks.name_key == io.name_key,
        _ => false,
    }
}

/// Whether `plan` is a shape [`project_columnar`](Engine::project_columnar) specializes: a bare-column
/// projection over a single base table with no join / aggregate / window / DISTINCT / ORDER BY / LIMIT /
/// OFFSET and no index/GIN/GiST bound — a plain `SELECT c0, c3, … FROM t [WHERE …]` whose output is the
/// (optionally filtered) scan-order rows narrowed to a column subset. A residual filter is allowed (A3):
/// `project_columnar` applies it over the lanes into a selection vector. Pure plan inspection (charges
/// nothing), so a bail is free and the general materialize path runs with identical results + cost; the
/// store / paging / spillable / column-range gates live in `project_columnar`, which declines to that
/// path. LIMIT/OFFSET is excluded deliberately: a LIMIT with no ORDER BY streams with an early exit
/// ([`streaming_scan_eligible`]), which the whole-table gather must not steal.
pub(crate) fn vectorized_project_eligible(plan: &SelectPlan) -> bool {
    if plan.is_agg || plan.has_window || plan.distinct {
        return false;
    }
    if plan.rels.len() != 1 || !plan.joins.is_empty() {
        return false;
    }
    let rel = &plan.rels[0];
    if rel.srf.is_some() || rel.cte.is_some() || rel.derived.is_some() || rel.lateral {
        return false;
    }
    // No ORDER BY / LIMIT / OFFSET (those route to a streaming / sort / index path). A residual filter is
    // fine — project_columnar vectorizes it (A3).
    if !plan.order.is_empty() || plan.limit.is_some() || plan.offset.is_some() {
        return false;
    }
    // Full scan or a primary-key bound only — an index / GIN / GiST bound changes the scan mechanics.
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
    // Every projection must be a bare column reference: a bare `RExpr::Column` evaluates to `row[index]`
    // with zero operator_eval, so gathering it from a dense lane is cost-identical. An expression
    // projection (`c0 + 1`, a function call) charges operator_eval and needs a row — it keeps the row path.
    if plan.projections.is_empty() {
        return false;
    }
    plan.projections
        .iter()
        .all(|p| matches!(p, RExpr::Column(_)))
}

/// Evaluate `filter` over the gathered per-column lanes and return the surviving row indices (the
/// selection vector) — filter vectorization (packed-leaf.md §11 Track A3). It reuses the scalar
/// [`RExpr::eval`] verbatim over a SINGLE reusable scratch row (the masked columns filled from the lanes
/// at that row index, untouched columns left `Null`), so the predicate's `operator_eval` charges and its
/// 3VL survivor test (keep iff `TRUE`) are byte-identical to the scalar `WHERE` loop — and the result is
/// identical too, because the row path also feeds the filter a MASKED row (untouched columns `Null` via
/// resolve_columns / row_at_masked) and the filter references only masked columns (`collect_touched`
/// includes the filter), so a scratch row filled from the lanes is the same input. The one reusable
/// scratch row is the allocation win: no full-width row per scanned row, only the `i32` survivor indices.
/// The caller has verified no touched column spills, so every masked lane is a non-empty `Vec<Value>` of
/// length `row_count` (an untouched column's lane stays empty but is never read).
pub(crate) fn filter_columnar(
    filter: &RExpr,
    cols: &[Vec<Value>],
    mask: &[bool],
    row_count: usize,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Vec<i32>> {
    let mut sel = Vec::new();
    let mut scratch: Vec<Value> = vec![Value::Null; mask.len()];
    for i in 0..row_count {
        for (c, &m) in mask.iter().enumerate() {
            if m {
                scratch[c] = cols[c][i].clone();
            }
        }
        if filter.eval(&scratch, env, meter)?.is_true() {
            sel.push(i as i32);
        }
    }
    Ok(sel)
}

/// Whether one aggregate is a specialized numeric kernel the vectorized aggregate path folds: a plain
/// (non-DISTINCT, non-FILTER, non-ordered-set, non-hypothetical) `COUNT(*)` / `COUNT(col)` /
/// `SUM(i16|i32)` / `SUM`|`AVG(f32|f64)` / `MIN(col)` / `MAX(col)` whose operand (where it has one) is
/// a bare column reference. `SUM(i64|decimal)` and `AVG(decimal)` are deferred (their fold charges
/// running-sum-dependent decimal_work); `MIN`/`MAX` fold ANY type through `value_cmp`. Reusing the
/// shared [`Acc::fold`] keeps the fold byte-identical to the scalar path (the scalar grouped path folds
/// through the same `Acc::fold`), so only the group/scan machinery differs.
pub(crate) fn vectorized_spec_eligible(spec: &AggSpec) -> bool {
    if spec.distinct || spec.filter.is_some() || spec.osa.is_some() || spec.hypo.is_some() {
        return false;
    }
    match spec.plan {
        AggPlan::CountStar => spec.operand.is_none(),
        AggPlan::Count
        | AggPlan::SumInt
        | AggPlan::SumFloat(_)
        | AggPlan::AvgFloat(_)
        | AggPlan::Min
        | AggPlan::Max => matches!(spec.operand, Some(RExpr::Column(_))),
        _ => false,
    }
}

/// The bare-column ordinal an eligible aggregate reads (its operand `RExpr::Column(idx)`), or `None`
/// for `COUNT(*)` (which folds no value). Eligibility ([`vectorized_spec_eligible`]) guarantees the
/// operand is either absent or a bare column, so this is total over an eligible spec.
pub(crate) fn operand_col(spec: &AggSpec) -> Option<usize> {
    match &spec.operand {
        Some(RExpr::Column(i)) => Some(*i),
        _ => None,
    }
}

/// The survivor value source for the vectorized fold — the ONE seam that differs between the row path
/// (a `Vec<Row>` of full rows) and the columnar path (dense per-column lanes + an optional A3 selection
/// vector). `at(j, col)` reads survivor `j`'s value in column `col`, so the fold kernels below are
/// written once and run either way. Cost is unaffected: both feed the same values in scan order.
pub(crate) enum LaneSrc<'a> {
    /// The row path: survivors are full rows; `at(j, col)` is `rows[j][col]`.
    Rows(&'a [Row]),
    /// The columnar path: `cols[col]` is a dense lane; `sel` (A3) maps survivor `j` to lane index
    /// `sel[j]` (or `j` itself when there is no filter).
    Cols {
        cols: &'a [Vec<Value>],
        sel: Option<&'a [i32]>,
    },
}

#[cfg(test)]
mod candidate_inventory_tests {
    use super::*;
    use crate::ast::Statement;

    /// Inventory is an internal planner invariant the SQL corpus cannot render: EXPLAIN shows only
    /// the selected path. Mirrors the Go/TS white-box case with two eligible indexes of every kind.
    #[test]
    fn scan_candidate_inventory_is_complete_canonical_and_legacy_neutral() {
        let mut db = Engine::new();
        for sql in [
            "CREATE TABLE inventory (id i32 PRIMARY KEY, a i32, b i32, tags i32[], span i32range)",
            "CREATE INDEX z_btree ON inventory (b)",
            "CREATE INDEX a_btree ON inventory (a)",
            "CREATE INDEX z_gin ON inventory USING gin (tags)",
            "CREATE INDEX a_gin ON inventory USING gin (tags)",
            "CREATE INDEX z_gist ON inventory USING gist (span)",
            "CREATE INDEX a_gist ON inventory USING gist (span)",
            "INSERT INTO inventory VALUES (1, 1, 1, '{1}', '[1,3)')",
            "INSERT INTO inventory VALUES (2, 2, 2, '{1,2}', '[2,4)')",
            "INSERT INTO inventory VALUES (3, 3, 3, '{3}', '[5,8)')",
            "INSERT INTO inventory VALUES (4, 4, 4, '{4}', '[9,12)')",
        ] {
            crate::execute(&mut db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }

        let filter = planned_inventory_filter(
            &db,
            "SELECT id FROM inventory WHERE \
             (id = 1 OR id = 2) AND id >= 0 AND \
             (a = 1 OR a = 2) AND a >= 0 AND \
             (b = 1 OR b = 2) AND b >= 0 AND \
             tags @> ARRAY[1] AND span && i32range(1, 3)",
        );
        let mut table = db.table("inventory").expect("inventory table").clone();
        // Deliberately scramble the catalog slice: canonical identity, never container iteration,
        // determines inventory order.
        table.indexes.reverse();
        let rel = ScopeRel {
            label: "inventory".to_string(),
            table: &table,
            offset: 0,
            qualifier_only: false,
            cte: None,
            db: None,
        };
        let candidates = inventory_scan_candidates(Some(&filter), &rel, &db);
        let got: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.identity.to_string())
            .collect();
        let want = vec![
            "pk",
            "btree:a_btree",
            "btree:z_btree",
            "gist:a_gist",
            "gist:z_gist",
            "gin:a_gin",
            "gin:z_gin",
            "pk_interval",
            "index_interval:a_btree",
            "index_interval:z_btree",
            "full",
        ];
        assert_eq!(got, want);
        let estimates = estimate_scan_candidates(&candidates, &rel, &db, true);
        assert_eq!(estimates.len(), candidates.len());
        let logical_rows = estimates[0].rows;
        for (candidate, estimate) in candidates.iter().zip(&estimates) {
            assert_eq!(
                estimate.rows, logical_rows,
                "{} logical rows",
                candidate.identity
            );
            assert_eq!(
                estimate.tie_key,
                crate::estimator::candidate_tie_key(
                    crate::estimator_constants::ACCESS_PATH_ORDER[candidate.identity.kind as usize],
                    &candidate.identity.index_name,
                )
            );
            assert!(estimate.cost >= 0);
        }
        for (sql, expected_rows, empty_candidate) in [
            (
                "SELECT id FROM inventory WHERE a IN (1, 1, 1, 1, 1)",
                1,
                None,
            ),
            (
                "SELECT id FROM inventory WHERE a = NULL",
                0,
                Some("btree:a_btree"),
            ),
            (
                "SELECT id FROM inventory WHERE a = 1 AND a = 2",
                0,
                Some("btree:a_btree"),
            ),
            (
                "SELECT id FROM inventory WHERE a > 3 AND a < 2",
                0,
                Some("btree:a_btree"),
            ),
        ] {
            let filter = planned_inventory_filter(&mut db, sql);
            let shape_candidates = inventory_scan_candidates(Some(&filter), &rel, &db);
            for (candidate, estimate) in shape_candidates.iter().zip(estimate_scan_candidates(
                &shape_candidates,
                &rel,
                &db,
                true,
            )) {
                assert_eq!(
                    estimate.rows, expected_rows,
                    "{sql} {} logical rows",
                    candidate.identity
                );
                if empty_candidate == Some(candidate.identity.to_string().as_str()) {
                    assert_eq!(estimate.cost, 0, "{sql} {empty_candidate:?} empty access");
                }
            }
        }
        let full_candidates = inventory_scan_candidates(None, &rel, &db);
        let full_estimate = estimate_scan_candidates(&full_candidates, &rel, &db, true);
        let full_actual = crate::execute(&mut db, "SELECT id FROM inventory").unwrap();
        assert_eq!(full_estimate[0].cost, full_actual.cost());
        for candidate in &candidates {
            assert!(std::ptr::eq(candidate.residual.unwrap(), &filter));
            assert_eq!(
                candidate.bound.is_none(),
                candidate.identity.kind == ScanCandidateKind::Full,
                "{} bound shape",
                candidate.identity
            );
            match candidate.identity.kind {
                ScanCandidateKind::Btree | ScanCandidateKind::IndexInterval => {
                    assert!(matches!(
                        &candidate.scan_order,
                        ScanOrderCapability::IndexKey { index_name, reversible: false }
                            if index_name == &candidate.identity.index_name
                    ));
                }
                _ => assert!(matches!(
                    candidate.scan_order,
                    ScanOrderCapability::StorageKey { reversible: true }
                )),
            }
        }
        // The direct >= conjuncts clip their OR unions. Preserve the pre-P3 exception where the
        // clipped PK set replaces the broader contiguous PK bound.
        assert!(matches!(
            select_legacy_scan_candidate(candidates, SELECT_SCAN_BOUND_POLICY),
            Some(ScanBound::PkSet(_))
        ));

        let index_clip_filter = planned_inventory_filter(
            &db,
            "SELECT id FROM inventory WHERE \
			 (a = 1 OR a = 2) AND a >= 0 AND \
			 (b = 1 OR b = 2) AND b >= 0",
        );
        let selected = select_legacy_scan_candidate(
            inventory_scan_candidates(Some(&index_clip_filter), &rel, &db),
            SELECT_SCAN_BOUND_POLICY,
        );
        assert!(matches!(
            selected,
            Some(ScanBound::IndexSet(set)) if set.name_key == "a_btree"
        ));

        let opclass_filter = planned_inventory_filter(
            &db,
            "SELECT id FROM inventory WHERE tags @> ARRAY[1] AND span && i32range(1, 3)",
        );
        let selected = select_legacy_scan_candidate(
            inventory_scan_candidates(Some(&opclass_filter), &rel, &db),
            SELECT_SCAN_BOUND_POLICY,
        );
        assert!(matches!(selected, Some(ScanBound::Gist(g)) if g.name_key == "a_gist"));
        let selected = select_legacy_scan_candidate(
            inventory_scan_candidates(Some(&opclass_filter), &rel, &db),
            MUTATION_SCAN_BOUND_POLICY,
        );
        assert!(matches!(selected, Some(ScanBound::Gin(g)) if g.name_key == "a_gin"));
    }

    fn planned_inventory_filter(db: &Engine, sql: &str) -> RExpr {
        let Statement::Select(select) = db.parse(sql).unwrap() else {
            panic!("inventory query did not parse as SELECT");
        };
        db.plan_select(&select, None, &[], &mut ParamTypes::default())
            .unwrap()
            .filter
            .expect("inventory query filter")
    }
}
