//! P9 deterministic column statistics collection and snapshot state.

use super::*;
use crate::estimator_constants::{
    SELECTIVITY_INEQUALITY, SELECTIVITY_PAIRED_RANGE, STATISTICS_HISTOGRAM_BOUNDS,
    STATISTICS_KMV_HASHES, STATISTICS_MAX_VALUE_BYTES, STATISTICS_MCV_ENTRIES,
    STATISTICS_NDV_SCALE_DENOMINATOR, STATISTICS_NDV_SCALE_NUMERATOR, STATISTICS_SAMPLE_ROWS,
    STATISTICS_TERM_BUCKET_WIDTH, STATISTICS_TERM_ENTRIES,
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Clone, Debug)]
pub(crate) struct StatisticsValue {
    pub(crate) value: Value,
    pub(crate) key: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct StatisticsMcv {
    pub(crate) value: StatisticsValue,
    pub(crate) frequency: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct ColumnStatistics {
    pub(crate) analyzed_rows: i64,
    pub(crate) stale: bool,
    pub(crate) null_count: i64,
    pub(crate) width_sum: i64,
    pub(crate) distinct_count: Option<i64>,
    pub(crate) sample_rows: u32,
    pub(crate) sample_nonnull_rows: u32,
    pub(crate) mcv: Vec<StatisticsMcv>,
    pub(crate) histogram: Vec<StatisticsValue>,
    /// GIN term facts, present exactly for a term-eligible column (statistics.md §4.5).
    pub(crate) terms: Option<TermStatistics>,
}

/// A term-eligible column's GIN term facts (statistics.md §4.5): the saturated count of term
/// occurrences, the row-count estimate for an unlisted term (0 = the list is complete), and the
/// most common terms with their row counts, in descending count then ascending byte order.
#[derive(Clone, Debug, Default)]
pub(crate) struct TermStatistics {
    pub(crate) occurrences: i64,
    pub(crate) untracked_rows: i64,
    pub(crate) entries: Vec<(Vec<u8>, i64)>,
}

/// Does this column type have a GIN operator class (statistics.md §4.5)? `jsonb`, or an array whose
/// element type a GIN index admits.
pub(crate) fn term_eligible(ty: &Type) -> bool {
    ty.is_jsonb()
        || ty
            .array_element()
            .is_some_and(|elem| is_gin_element_type(&elem))
}

/// PostgreSQL's lossy counting over GIN term occurrences (statistics.md §4.5), bucket width
/// `STATISTICS_TERM_BUCKET_WIDTH`. Removal is a per-entry predicate, so the map's iteration order
/// never reaches the result.
#[derive(Default)]
struct TermCounter {
    occurrences: i64,
    entries: HashMap<Vec<u8>, (i64, i64)>,
    pruned: bool,
    oversized: bool,
}

impl TermCounter {
    fn add(&mut self, term: &[u8]) {
        self.occurrences = self.occurrences.saturating_add(1);
        let bucket =
            (self.occurrences + STATISTICS_TERM_BUCKET_WIDTH - 1) / STATISTICS_TERM_BUCKET_WIDTH;
        if term.len() > STATISTICS_MAX_VALUE_BYTES {
            self.oversized = true;
        } else if let Some(entry) = self.entries.get_mut(term) {
            entry.0 += 1;
        } else {
            self.entries.insert(term.to_vec(), (1, bucket - 1));
        }
        if self.occurrences % STATISTICS_TERM_BUCKET_WIDTH == 0 {
            let before = self.entries.len();
            self.entries
                .retain(|_, (count, delta)| *count + *delta > bucket);
            self.pruned |= self.entries.len() != before;
        }
    }

    fn finish(self) -> TermStatistics {
        let mut ranked: Vec<(Vec<u8>, i64)> = self
            .entries
            .into_iter()
            .map(|(term, (count, _))| (term, count))
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut untracked = 0i64;
        if let Some((_, count)) = ranked.get(STATISTICS_TERM_ENTRIES) {
            untracked = untracked.max(*count);
        }
        if self.pruned {
            untracked = untracked.max(
                self.occurrences / STATISTICS_TERM_BUCKET_WIDTH
                    + i64::from(self.occurrences % STATISTICS_TERM_BUCKET_WIDTH != 0),
            );
        }
        if self.oversized {
            untracked = untracked.max(1);
        }
        ranked.truncate(STATISTICS_TERM_ENTRIES);
        TermStatistics {
            occurrences: self.occurrences,
            untracked_rows: untracked,
            entries: ranked,
        }
    }
}

#[derive(Debug)]
struct SampleRow {
    priority: u64,
    ordinal: u64,
    nonnull: bool,
    oversized: bool,
    retained: Option<StatisticsValue>,
}

impl PartialEq for SampleRow {
    fn eq(&self, other: &Self) -> bool {
        (self.priority, self.ordinal) == (other.priority, other.ordinal)
    }
}
impl Eq for SampleRow {}
impl PartialOrd for SampleRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SampleRow {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.priority, self.ordinal).cmp(&(other.priority, other.ordinal))
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) fn distribution_eligible(ty: &Type) -> bool {
    match ty {
        Type::Scalar(ScalarType::Json | ScalarType::Jsonb | ScalarType::JsonPath) => false,
        Type::Scalar(_) | Type::Range(_) => true,
        Type::Composite(_) | Type::Array(_) | Type::Record(_) => false,
    }
}

fn retain_lowest_sample(heap: &mut BinaryHeap<SampleRow>, row: SampleRow) {
    if heap.len() < STATISTICS_SAMPLE_ROWS {
        heap.push(row);
    } else if heap.peek().is_some_and(|largest| row < *largest) {
        heap.pop();
        heap.push(row);
    }
}

fn retain_kmv(heap: &mut BinaryHeap<u64>, seen: &mut HashSet<u64>, hash: u64) {
    if seen.contains(&hash) {
        return;
    }
    if heap.len() < STATISTICS_KMV_HASHES {
        heap.push(hash);
        seen.insert(hash);
    } else if heap.peek().is_some_and(|largest| hash < *largest) {
        let removed = heap.pop().expect("nonempty KMV heap");
        seen.remove(&removed);
        heap.push(hash);
        seen.insert(hash);
    }
}

fn kmv_count(heap: &BinaryHeap<u64>, nonnull_rows: i64) -> i64 {
    if heap.len() < STATISTICS_KMV_HASHES {
        return heap.len() as i64;
    }
    let r = u128::from(*heap.peek().expect("full KMV heap"));
    let numerator = (STATISTICS_KMV_HASHES as u128 - 1) << 64;
    let denominator = r + 1;
    let estimate = numerator.div_ceil(denominator);
    estimate
        .max(STATISTICS_KMV_HASHES as u128 + 1)
        .min(nonnull_rows.max(0) as u128) as i64
}

fn finish_distribution(
    mut sample: Vec<SampleRow>,
    analyzed_rows: i64,
    distinct_count: i64,
) -> (u32, Vec<StatisticsMcv>, Vec<StatisticsValue>) {
    let sample_nonnull = sample.iter().filter(|row| row.nonnull).count() as u32;
    if sample_nonnull == 0 {
        return (0, Vec::new(), Vec::new());
    }
    let has_oversized = sample.iter().any(|row| row.nonnull && row.oversized);
    let mut retained: Vec<StatisticsValue> =
        sample.drain(..).filter_map(|row| row.retained).collect();
    retained.sort_by(|a, b| a.key.cmp(&b.key));

    #[derive(Clone)]
    struct Group {
        value: StatisticsValue,
        frequency: u32,
    }
    let mut groups: Vec<Group> = Vec::new();
    for value in retained {
        if let Some(last) = groups.last_mut()
            && last.value.key == value.key
        {
            last.frequency += 1;
        } else {
            groups.push(Group {
                value,
                frequency: 1,
            });
        }
    }

    let complete = analyzed_rows <= STATISTICS_SAMPLE_ROWS as i64;
    let all_groups = complete && !has_oversized && groups.len() <= STATISTICS_MCV_ENTRIES;
    let mut selected: Vec<Group> = groups
        .iter()
        .filter(|group| {
            all_groups
                || (group.frequency >= 2
                    && i64::from(group.frequency).saturating_mul(distinct_count)
                        > i64::from(sample_nonnull))
        })
        .cloned()
        .collect();
    selected.sort_by(|a, b| {
        b.frequency
            .cmp(&a.frequency)
            .then_with(|| a.value.key.cmp(&b.value.key))
    });
    selected.truncate(STATISTICS_MCV_ENTRIES);
    let selected_keys: HashSet<Vec<u8>> = selected.iter().map(|g| g.value.key.clone()).collect();
    let mcv = selected
        .into_iter()
        .map(|group| StatisticsMcv {
            value: group.value,
            frequency: group.frequency,
        })
        .collect();

    if has_oversized {
        return (sample_nonnull, mcv, Vec::new());
    }
    let remaining: Vec<StatisticsValue> = groups
        .into_iter()
        .filter(|group| !selected_keys.contains(&group.value.key))
        .flat_map(|group| std::iter::repeat_n(group.value, group.frequency as usize))
        .collect();
    if remaining.len() < 2 {
        return (sample_nonnull, mcv, Vec::new());
    }
    let bounds = STATISTICS_HISTOGRAM_BOUNDS.min(remaining.len());
    let histogram = (0..bounds)
        .map(|i| {
            let rank = i * (remaining.len() - 1) / (bounds - 1);
            remaining[rank].clone()
        })
        .collect();
    (sample_nonnull, mcv, histogram)
}

/// One analyzed column scaled to the table's exact current row count. The owned key vectors keep
/// planner consumers independent of snapshot borrow lifetimes and make every fold use persisted
/// order, not map order.
pub(crate) struct CurrentColumnStatistics<'a> {
    pub(crate) rows: i64,
    pub(crate) null_rows: i64,
    pub(crate) nonnull_rows: i64,
    pub(crate) ndv: Option<i64>,
    pub(crate) average_width: Option<i64>,
    // The keys borrow the snapshot's immutable fact: planning reads them on every estimate, so a
    // copy per call dominated the planning of a statement over analyzed columns.
    mcv: Vec<(&'a [u8], i64)>,
    histogram: Vec<&'a [u8]>,
    complete_mcv: bool,
}

fn statistics_fraction(rows: i64, population: i64) -> crate::estimator::Selectivity {
    use crate::estimator::Selectivity;
    if rows <= 0 || population <= 0 {
        Selectivity::Zero
    } else if rows >= population {
        Selectivity::All
    } else {
        Selectivity::fraction(crate::estimator_constants::EstimatorFraction {
            numerator: rows,
            denominator: population,
        })
    }
}

pub(crate) fn statistics_scale(n: i64, numerator: i64, denominator: i64) -> i64 {
    if n <= 0 || numerator <= 0 || denominator <= 0 {
        0
    } else {
        crate::estimator::scale_ceil(
            n,
            crate::estimator_constants::EstimatorFraction {
                numerator,
                denominator,
            },
        )
    }
}

pub(crate) fn current_column_statistics<'a>(
    rel: &ScopeRel<'_>,
    column: usize,
    catalog: &'a Engine,
) -> Option<CurrentColumnStatistics<'a>> {
    let fact = catalog.column_statistics_scoped(rel.db.as_deref(), &rel.table.name, column)?;
    let rows = catalog
        .store_scoped(rel.db.as_deref(), &rel.table.name)
        .count()
        .unwrap_or(0);
    if fact.analyzed_rows == 0 && rows != 0 {
        return None;
    }
    let null_rows = if fact.analyzed_rows == 0 {
        0
    } else {
        statistics_scale(rows, fact.null_count, fact.analyzed_rows).min(rows)
    };
    let nonnull_rows = rows.saturating_sub(null_rows);
    let analyzed_nonnull = fact.analyzed_rows.saturating_sub(fact.null_count);
    let ndv = fact.distinct_count.map(|distinct| {
        if analyzed_nonnull == 0 {
            0
        } else if i128::from(distinct) * i128::from(STATISTICS_NDV_SCALE_DENOMINATOR)
            > i128::from(analyzed_nonnull) * i128::from(STATISTICS_NDV_SCALE_NUMERATOR)
        {
            statistics_scale(nonnull_rows, distinct, analyzed_nonnull).min(nonnull_rows)
        } else {
            distinct.min(nonnull_rows)
        }
    });
    let average_width = (analyzed_nonnull > 0).then(|| {
        fact.width_sum / analyzed_nonnull + i64::from(fact.width_sum % analyzed_nonnull != 0)
    });
    let mut remaining = nonnull_rows;
    let mut mcv = Vec::with_capacity(fact.mcv.len());
    for entry in &fact.mcv {
        let scaled = statistics_scale(
            rows,
            i64::from(entry.frequency),
            i64::from(fact.sample_rows),
        )
        .min(remaining);
        remaining -= scaled;
        mcv.push((entry.value.key.as_slice(), scaled));
    }
    let sampled_mcv_rows: u64 = fact
        .mcv
        .iter()
        .map(|entry| u64::from(entry.frequency))
        .sum();
    let complete_mcv = !fact.stale
        && i64::from(fact.sample_rows) == fact.analyzed_rows
        && sampled_mcv_rows == u64::from(fact.sample_nonnull_rows);
    Some(CurrentColumnStatistics {
        rows,
        null_rows,
        nonnull_rows,
        ndv,
        average_width,
        mcv,
        histogram: fact
            .histogram
            .iter()
            .map(|bound| bound.key.as_slice())
            .collect(),
        complete_mcv,
    })
}

/// A GIN-served predicate's estimate from term statistics (statistics.md §6.1): the rows it admits,
/// the posting-list entries a GIN gather reads, and the estimated total entries in the index.
pub(crate) struct GinTermEstimate {
    pub(crate) rows: i64,
    pub(crate) entries: i64,
    pub(crate) total_entries: i64,
}

/// Price a GIN predicate over the local `column` with a literal `operand` from the column's term
/// facts (statistics.md §6.1). `None` — the structural default applies — when the column has no
/// term facts, the operand is not a plan-time literal, or the query falls back to a full scan.
pub(crate) fn statistics_gin_estimate(
    rel: &ScopeRel<'_>,
    column: usize,
    opclass: GinOpclass,
    strategy: GinStrategy,
    operand: &RExpr,
    catalog: &Engine,
) -> Option<GinTermEstimate> {
    let fact = catalog.column_statistics_scoped(rel.db.as_deref(), &rel.table.name, column)?;
    let term_facts = fact.terms.as_ref()?;
    let value = statistics_gin_operand_value(operand)?;
    let current = current_column_statistics(rel, column, catalog)?;
    let n = current.rows;
    let total_entries = statistics_scale(n, term_facts.occurrences, fact.analyzed_rows);
    let tree = match gin_query_terms(opclass, strategy, &value) {
        GinQuery::Empty => {
            return Some(GinTermEstimate {
                rows: 0,
                entries: 0,
                total_entries,
            });
        }
        GinQuery::FullScan => return None,
        GinQuery::Terms(terms) => {
            crate::gin_jsonpath::GinTree::from_terms(strategy.is_any(), terms)
        }
        GinQuery::Tree(tree) => tree,
    };
    let term_rows = |term: &[u8]| -> i64 {
        if current.nonnull_rows == 0 {
            return 0;
        }
        if let Some((_, count)) = term_facts.entries.iter().find(|(t, _)| t == term) {
            return statistics_scale(n, *count, fact.analyzed_rows).min(current.nonnull_rows);
        }
        if term_facts.untracked_rows == 0 && !fact.stale {
            return 0;
        }
        let half = term_facts.untracked_rows / 2 + term_facts.untracked_rows % 2;
        statistics_scale(n, half, fact.analyzed_rows)
            .min(current.nonnull_rows)
            .max(1)
    };
    let (rows, entries) = gin_tree_estimate(&tree, &term_rows, n, current.nonnull_rows);
    Some(GinTermEstimate {
        rows,
        entries,
        total_entries,
    })
}

/// A GIN query tree's `(rows, gin_entry)` estimate (statistics.md §6.1), following the gather's
/// evaluation (gin.md §2.2/§6): a term reads its rows. An `Or` sums its leaves and children (rows
/// capped at the non-NULL rows). An `And` folds its leaves rarest-first under independence — the
/// first read whole, each later term probed at the survivors — then each child in order, whose
/// entries add whole and whose rows scale the survivors.
fn gin_tree_estimate(
    tree: &crate::gin_jsonpath::GinTree,
    term_rows: &impl Fn(&[u8]) -> i64,
    n: i64,
    nonnull: i64,
) -> (i64, i64) {
    use crate::gin_jsonpath::GinTree;
    match tree {
        GinTree::Term(term) => {
            let rows = term_rows(term);
            (rows, rows)
        }
        GinTree::Or(leaves, rest) => {
            let mut sum = 0i64;
            let mut entries = 0i64;
            for term in leaves {
                let rows = term_rows(term);
                sum = crate::estimator::sat_add(sum, rows);
                entries = crate::estimator::sat_add(entries, rows);
            }
            for child in rest {
                let (rows, e) = gin_tree_estimate(child, term_rows, n, nonnull);
                sum = crate::estimator::sat_add(sum, rows);
                entries = crate::estimator::sat_add(entries, e);
            }
            (sum.min(nonnull), entries)
        }
        GinTree::And(leaves, rest) => {
            let mut priced: Vec<(i64, &Vec<u8>)> =
                leaves.iter().map(|term| (term_rows(term), term)).collect();
            priced.sort();
            let mut rest = rest.iter();
            let (mut rows, mut entries) = match priced.split_first() {
                Some((&(first, _), tail)) => {
                    let mut rows = first;
                    let mut entries = first;
                    for &(term_rows, _) in tail {
                        entries = crate::estimator::sat_add(entries, rows);
                        rows = statistics_scale(rows, term_rows, n);
                    }
                    (rows, entries)
                }
                None => match rest.next() {
                    Some(first) => gin_tree_estimate(first, term_rows, n, nonnull),
                    None => (0, 0),
                },
            };
            for child in rest {
                let (child_rows, e) = gin_tree_estimate(child, term_rows, n, nonnull);
                entries = crate::estimator::sat_add(entries, e);
                rows = statistics_scale(rows, child_rows, n);
            }
            (rows, entries)
        }
    }
}

/// A term-eligible column's estimated total GIN term occurrences at its current row count
/// (statistics.md §6.1), or `None` without term facts.
pub(crate) fn statistics_gin_total_entries(
    rel: &ScopeRel<'_>,
    column: usize,
    catalog: &Engine,
) -> Option<i64> {
    let fact = catalog.column_statistics_scoped(rel.db.as_deref(), &rel.table.name, column)?;
    let term_facts = fact.terms.as_ref()?;
    let current = current_column_statistics(rel, column, catalog)?;
    Some(statistics_scale(
        current.rows,
        term_facts.occurrences,
        fact.analyzed_rows,
    ))
}

/// A GIN query operand's plan-time value (statistics.md §6.1): a literal, a folded array literal, or
/// a one-dimensional `ARRAY[...]` of literals.
fn statistics_gin_operand_value(operand: &RExpr) -> Option<Value> {
    match operand {
        RExpr::Array {
            elems,
            nested: false,
        } => elems
            .iter()
            .map(|elem| estimator_value(elem).and_then(|c| rexpr_const_to_value(&c).ok()))
            .collect::<Option<Vec<_>>>()
            .map(|values| Value::Array(ArrayVal::one_dim(values))),
        other => {
            let value = estimator_value(other)?;
            match value.as_ref() {
                RExpr::ConstArray(array) => Some(Value::Array((**array).clone())),
                constant => rexpr_const_to_value(constant).ok(),
            }
        }
    }
}

/// The logical selectivity of a GIN-served leaf from term statistics (statistics.md §6.1). Array
/// `=` keeps its structural equality estimate as a logical leaf.
fn statistics_gin_leaf_selectivity(
    expr: &RExpr,
    rel: &ScopeRel<'_>,
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    if !matches!(
        expr,
        RExpr::ArrayFunc { .. }
            | RExpr::Quantified { .. }
            | RExpr::JsonContains { .. }
            | RExpr::JsonHasKey { .. }
            | RExpr::JsonPathFn { .. }
    ) {
        return None;
    }
    for (column, declared) in rel.table.columns.iter().enumerate() {
        if !term_eligible(&declared.ty) {
            continue;
        }
        let opclass = GinOpclass::for_type(&declared.ty)?;
        let Some((strategy, operand)) = gin_match(expr, rel.offset + column, opclass) else {
            continue;
        };
        if strategy == GinStrategy::Equal {
            return None;
        }
        let estimate = statistics_gin_estimate(rel, column, opclass, strategy, operand, catalog)?;
        let current = current_column_statistics(rel, column, catalog)?;
        return Some(statistics_fraction(estimate.rows, current.rows));
    }
    None
}

fn statistics_column(expr: &RExpr, rel: &ScopeRel<'_>) -> Option<usize> {
    let RExpr::Column(global) = expr else {
        return None;
    };
    statistics_local_column(*global, rel.offset, rel.table.columns.len())
}

fn statistics_local_column(global: usize, offset: usize, column_count: usize) -> Option<usize> {
    let local = global.checked_sub(offset)?;
    (local < column_count).then_some(local)
}

fn statistics_reverse_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

fn statistics_comparison<'a>(
    expr: &'a RExpr,
    rel: &ScopeRel<'_>,
) -> Option<(usize, CmpOp, &'a RExpr)> {
    let RExpr::Compare { op, lhs, rhs, .. } = expr else {
        return None;
    };
    if let Some(column) = statistics_column(lhs, rel)
        && (rexpr_const_to_value(rhs).is_ok() || matches!(rhs.as_ref(), RExpr::Param(_)))
    {
        return Some((column, *op, rhs));
    }
    statistics_column(rhs, rel)
        .filter(|_| rexpr_const_to_value(lhs).is_ok() || matches!(lhs.as_ref(), RExpr::Param(_)))
        .map(|column| (column, statistics_reverse_op(*op), lhs.as_ref()))
}

fn statistics_literal_key(
    literal: &RExpr,
    rel: &ScopeRel<'_>,
    column: usize,
    catalog: &Engine,
) -> Option<Vec<u8>> {
    let literal = estimator_value(literal)?;
    let value = statistics_key_value(
        rexpr_const_to_value(&literal).ok()?,
        &rel.table.columns[column].ty,
    )?;
    if matches!(value, Value::Null) {
        return None;
    }
    let snap = catalog.relation_snap(rel.db.as_deref(), &rel.table.name);
    let collation = rel.table.columns[column]
        .collation
        .as_deref()
        .and_then(|name| snap.resolve_collation(name));
    encode_typed_key(&rel.table.columns[column].ty, &value, collation.as_deref()).ok()
}

fn statistics_value_key(
    value: &Value,
    rel: &ScopeRel<'_>,
    column: usize,
    catalog: &Engine,
) -> Option<Vec<u8>> {
    let value = statistics_key_value(value.clone(), &rel.table.columns[column].ty)?;
    let snap = catalog.relation_snap(rel.db.as_deref(), &rel.table.name);
    let collation = rel.table.columns[column]
        .collation
        .as_deref()
        .and_then(|name| snap.resolve_collation(name));
    encode_typed_key(&rel.table.columns[column].ty, &value, collation.as_deref()).ok()
}

/// Adapt a comparison literal to the column's stored-key family. Resolved numeric comparisons
/// admit the one cross-family promotion `integer -> decimal`, while their RExpr constants retain
/// the syntactic `Value::Int`; feeding that directly to a decimal key encoder is invalid. Every
/// other mismatched family falls back to the deterministic row-count selectivity.
fn statistics_key_value(value: Value, ty: &Type) -> Option<Value> {
    match (ty, value) {
        (_, Value::Null) => None,
        (Type::Scalar(ScalarType::Decimal), Value::Int(value)) => {
            Some(Value::Decimal(Decimal::from_i64(value)))
        }
        (Type::Scalar(scalar), value)
            if matches!(
                (scalar, &value),
                (ScalarType::Bool, Value::Bool(_))
                    | (
                        ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64,
                        Value::Int(_)
                    )
                    | (ScalarType::Text, Value::Text(_))
                    | (ScalarType::Decimal, Value::Decimal(_))
                    | (ScalarType::Bytea, Value::Bytea(_))
                    | (ScalarType::Uuid, Value::Uuid(_))
                    | (ScalarType::Timestamp, Value::Timestamp(_))
                    | (ScalarType::Timestamptz, Value::Timestamptz(_))
                    | (ScalarType::Date, Value::Date(_))
                    | (ScalarType::Interval, Value::Interval(_))
                    | (ScalarType::Float32, Value::Float32(_))
                    | (ScalarType::Float64, Value::Float64(_))
                    | (ScalarType::Json, Value::Json(_))
                    | (ScalarType::Jsonb, Value::Jsonb(_))
                    | (ScalarType::JsonPath, Value::JsonPath(_))
            ) =>
        {
            Some(value)
        }
        (Type::Range(_), value @ Value::Range(_)) => Some(value),
        _ => None,
    }
}

fn statistics_equality_rows(current: &CurrentColumnStatistics, key: &[u8]) -> i64 {
    if let Some((_, rows)) = current.mcv.iter().find(|(candidate, _)| *candidate == key) {
        return *rows;
    }
    if current.complete_mcv {
        return 0;
    }
    let mcv_rows = current
        .mcv
        .iter()
        .fold(0i64, |sum, (_, rows)| sum.saturating_add(*rows))
        .min(current.nonnull_rows);
    let residual = current.nonnull_rows - mcv_rows;
    let remaining_ndv = current
        .ndv
        .unwrap_or(0)
        .saturating_sub(current.mcv.len() as i64)
        .max(1);
    statistics_scale(residual, 1, remaining_ndv)
}

fn statistics_key_satisfies(order: std::cmp::Ordering, op: CmpOp) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match op {
        CmpOp::Eq => order == Equal,
        CmpOp::Ne => order != Equal,
        CmpOp::Lt => order == Less,
        CmpOp::Le => order != Greater,
        CmpOp::Gt => order == Greater,
        CmpOp::Ge => order != Less,
    }
}

fn statistics_less_rows(current: &CurrentColumnStatistics, key: &[u8], inclusive: bool) -> i64 {
    let op = if inclusive { CmpOp::Le } else { CmpOp::Lt };
    let mcv_rows = current.mcv.iter().fold(0i64, |sum, (candidate, rows)| {
        if statistics_key_satisfies((*candidate).cmp(key), op) {
            sum.saturating_add(*rows)
        } else {
            sum
        }
    });
    let all_mcv_rows = current
        .mcv
        .iter()
        .fold(0i64, |sum, (_, rows)| sum.saturating_add(*rows))
        .min(current.nonnull_rows);
    let residual = current.nonnull_rows - all_mcv_rows;
    let histogram_rows = if current.histogram.len() >= 2 {
        let ordinal = current
            .histogram
            .partition_point(|bound| (*bound) < key || (inclusive && (*bound) == key));
        statistics_scale(residual, ordinal as i64, current.histogram.len() as i64 - 1).min(residual)
    } else {
        statistics_scale(
            residual,
            SELECTIVITY_INEQUALITY.numerator,
            SELECTIVITY_INEQUALITY.denominator,
        )
    };
    mcv_rows
        .saturating_add(histogram_rows)
        .min(current.nonnull_rows)
}

fn statistics_comparison_rows(current: &CurrentColumnStatistics, op: CmpOp, key: &[u8]) -> i64 {
    match op {
        CmpOp::Eq => statistics_equality_rows(current, key),
        CmpOp::Ne => current
            .nonnull_rows
            .saturating_sub(statistics_equality_rows(current, key)),
        CmpOp::Lt => statistics_less_rows(current, key, false),
        CmpOp::Le => statistics_less_rows(current, key, true),
        CmpOp::Gt => current
            .nonnull_rows
            .saturating_sub(statistics_less_rows(current, key, true)),
        CmpOp::Ge => current
            .nonnull_rows
            .saturating_sub(statistics_less_rows(current, key, false)),
    }
}

fn statistics_bound_value(src: &BoundSrc, ty: ScalarType) -> Option<Value> {
    Some(match src {
        BoundSrc::Int(value) => Value::Int(*value),
        BoundSrc::Bool(value) => Value::Bool(*value),
        BoundSrc::Uuid(value) => Value::Uuid(*value),
        BoundSrc::Timestamp(value) if ty == ScalarType::Timestamptz => Value::Timestamptz(*value),
        BoundSrc::Timestamp(value) => Value::Timestamp(*value),
        BoundSrc::Date(value) => Value::Date(*value),
        BoundSrc::Text(value) => Value::Text(value.clone()),
        BoundSrc::Bytea(value) => Value::Bytea(value.clone()),
        BoundSrc::Decimal(value) => Value::Decimal(value.clone()),
        BoundSrc::Float32(value) => Value::Float32(*value),
        BoundSrc::Float64(value) => Value::Float64(*value),
        BoundSrc::Interval(value) => Value::Interval(*value),
        BoundSrc::Null => Value::Null,
        BoundSrc::Param(_) | BoundSrc::Outer { .. } | BoundSrc::Sibling(_) => return None,
    })
}

pub(crate) fn statistics_bound_source_selectivity(
    rel: &ScopeRel<'_>,
    column: usize,
    op: CmpOp,
    source: &BoundSrc,
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    let current = current_column_statistics(rel, column, catalog)?;
    if estimator_bound_is_null(source) {
        return Some(crate::estimator::Selectivity::Zero);
    }
    let key_type = rel.table.columns[column].ty.scalar();
    if matches!(source, BoundSrc::Param(_))
        && let Some(rows) = statistics_value_blind_rows(&current, op, rel, column)
    {
        return Some(statistics_fraction(rows, current.rows));
    }
    let Some(source) = estimator_bound_value(source, key_type) else {
        // An unbound parameter or a runtime outer/sibling column: the generic estimate.
        if op != CmpOp::Eq {
            return None;
        }
        return Some(statistics_fraction(
            statistics_scale(current.nonnull_rows, 1, current.ndv?.max(1)),
            current.rows,
        ));
    };
    let value = statistics_bound_value(&source, key_type)?;
    let key = statistics_value_key(&value, rel, column, catalog)?;
    Some(statistics_fraction(
        statistics_comparison_rows(&current, op, &key),
        current.rows,
    ))
}

pub(crate) fn statistics_bound_terms_selectivity(
    rel: &ScopeRel<'_>,
    column: usize,
    terms: &[BoundTerm],
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    if terms.is_empty() {
        return Some(crate::estimator::Selectivity::All);
    }
    if let Some(equality) = terms.iter().find(|term| term.op == CmpOp::Eq) {
        return statistics_bound_source_selectivity(rel, column, CmpOp::Eq, &equality.src, catalog);
    }
    let lower = terms
        .iter()
        .find(|term| matches!(term.op, CmpOp::Gt | CmpOp::Ge));
    let upper = terms
        .iter()
        .find(|term| matches!(term.op, CmpOp::Lt | CmpOp::Le));
    if let (Some(lower), Some(upper)) = (lower, upper) {
        let current = current_column_statistics(rel, column, catalog)?;
        let key_type = rel.table.columns[column].ty.scalar();
        let lower_value =
            statistics_bound_value(&*estimator_bound_value(&lower.src, key_type)?, key_type)?;
        let upper_value =
            statistics_bound_value(&*estimator_bound_value(&upper.src, key_type)?, key_type)?;
        let lower_key = statistics_value_key(&lower_value, rel, column, catalog)?;
        let upper_key = statistics_value_key(&upper_value, rel, column, catalog)?;
        let mcv_rows = current.mcv.iter().fold(0i64, |sum, (key, rows)| {
            if statistics_key_satisfies((*key).cmp(&lower_key), lower.op)
                && statistics_key_satisfies((*key).cmp(&upper_key), upper.op)
            {
                sum.saturating_add(*rows)
            } else {
                sum
            }
        });
        let all_mcv_rows = current
            .mcv
            .iter()
            .fold(0i64, |sum, (_, rows)| sum.saturating_add(*rows))
            .min(current.nonnull_rows);
        let residual = current.nonnull_rows - all_mcv_rows;
        let histogram_rows = if current.histogram.len() >= 2 {
            let lower_ordinal = current.histogram.partition_point(|bound| {
                (*bound) < lower_key.as_slice()
                    || (lower.op == CmpOp::Gt && (*bound) == lower_key.as_slice())
            });
            let upper_ordinal = current.histogram.partition_point(|bound| {
                (*bound) < upper_key.as_slice()
                    || (upper.op == CmpOp::Le && (*bound) == upper_key.as_slice())
            });
            statistics_scale(
                residual,
                upper_ordinal.saturating_sub(lower_ordinal) as i64,
                current.histogram.len() as i64 - 1,
            )
            .min(residual)
        } else {
            statistics_scale(
                residual,
                SELECTIVITY_PAIRED_RANGE.numerator,
                SELECTIVITY_PAIRED_RANGE.denominator,
            )
        };
        return Some(statistics_fraction(
            mcv_rows
                .saturating_add(histogram_rows)
                .min(current.nonnull_rows),
            current.rows,
        ));
    }
    let term = lower.or(upper)?;
    statistics_bound_source_selectivity(rel, column, term.op, &term.src, catalog)
}

/// Return a P9 leaf estimate when the expression is owned by this bare base-relation column.
pub(crate) fn statistics_leaf_selectivity(
    expr: &RExpr,
    rel: &ScopeRel<'_>,
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    if let Some(selectivity) = statistics_gin_leaf_selectivity(expr, rel, catalog) {
        return Some(selectivity);
    }
    if let RExpr::IsNull { operand, negated } = expr {
        let column = statistics_column(operand, rel)?;
        let current = current_column_statistics(rel, column, catalog)?;
        let rows = if *negated {
            current.nonnull_rows
        } else {
            current.null_rows
        };
        return Some(statistics_fraction(rows, current.rows));
    }
    if let RExpr::Column(_) = expr {
        let column = statistics_column(expr, rel)?;
        if !matches!(rel.table.columns[column].ty, Type::Scalar(ScalarType::Bool)) {
            return None;
        }
        let current = current_column_statistics(rel, column, catalog)?;
        let key = statistics_value_key(&Value::Bool(true), rel, column, catalog)?;
        return Some(statistics_fraction(
            statistics_equality_rows(&current, &key),
            current.rows,
        ));
    }
    if let RExpr::InValues {
        lhs, list, negated, ..
    } = expr
    {
        let column = statistics_column(lhs, rel)?;
        let current = current_column_statistics(rel, column, catalog)?;
        let mut seen = HashSet::new();
        let mut rows = 0i64;
        let mut has_null = false;
        for value in list {
            if matches!(value, Value::Null) {
                has_null = true;
                continue;
            }
            if let Some(key) = statistics_value_key(value, rel, column, catalog)
                && seen.insert(key.clone())
            {
                rows = rows
                    .saturating_add(statistics_equality_rows(&current, &key))
                    .min(current.nonnull_rows);
            }
        }
        if *negated {
            rows = if has_null {
                0
            } else {
                current.nonnull_rows.saturating_sub(rows)
            };
        }
        return Some(statistics_fraction(rows, current.rows));
    }
    if let RExpr::Quantified {
        lhs,
        array,
        elem_exact,
        elem_promoted,
        ..
    } = expr
        && (*elem_exact || *elem_promoted)
    {
        if estimator_any_array(expr) {
            let (current, rows, _) =
                statistics_array_equality_rows(lhs, array, *elem_promoted, rel, catalog)?;
            return Some(statistics_fraction(rows, current.rows));
        }
        // `column <> ALL(array)`: the non-NULL rows outside the `= ANY` rows, or none when the
        // array is NULL or holds a known NULL element.
        if estimator_not_all_array(expr) {
            let (current, rows, has_null) =
                statistics_array_equality_rows(lhs, array, *elem_promoted, rel, catalog)?;
            let rows = if has_null || estimator_null_array(array) {
                0
            } else {
                current.nonnull_rows.saturating_sub(rows)
            };
            return Some(statistics_fraction(rows, current.rows));
        }
    }
    let (column, op, literal) = statistics_comparison(expr, rel)?;
    let current = current_column_statistics(rel, column, catalog)?;
    if estimator_is_null(literal) {
        return Some(crate::estimator::Selectivity::Zero);
    }
    let rows = statistics_literal_rows(&current, op, literal, rel, column, catalog)?;
    Some(statistics_fraction(rows, current.rows))
}

/// The `column = ANY(array)` row count from column statistics (statistics.md §6), with the column's
/// statistics and whether the array holds a known NULL element: the IN-list rule over the array's
/// estimator elements — each distinct non-NULL element by statistics key adds its equality rows, a
/// NULL element adds none, and an element of unknown value adds the NDV average. A promoted element
/// type converts each known element to the column's type first, so an element no value can equal
/// adds none.
fn statistics_array_equality_rows<'a>(
    lhs: &RExpr,
    array: &RExpr,
    promoted: bool,
    rel: &ScopeRel<'_>,
    catalog: &'a Engine,
) -> Option<(CurrentColumnStatistics<'a>, i64, bool)> {
    use crate::estimator::sat_add;
    let column = statistics_column(lhs, rel)?;
    let current = current_column_statistics(rel, column, catalog)?;
    let mut seen = HashSet::new();
    let mut rows = 0i64;
    let mut has_null = false;
    let key_type = rel.table.columns[column].ty.as_scalar();
    for elem in estimator_array_elements(array) {
        let elem = match key_type {
            Some(key_type) if promoted => estimator_point_element(key_type, elem),
            _ => elem,
        };
        if estimator_is_null(&elem) {
            has_null = true;
            continue;
        }
        if let Some(key) = statistics_literal_key(&elem, rel, column, catalog) {
            if !seen.contains(&key) {
                rows = sat_add(rows, statistics_equality_rows(&current, &key));
                seen.insert(key);
            }
        } else {
            let ndv = current.ndv?;
            rows = sat_add(rows, statistics_scale(current.nonnull_rows, 1, ndv.max(1)));
        }
        rows = rows.min(current.nonnull_rows);
    }
    Some((current, rows, has_null))
}

/// The rows a column comparison with a non-NULL plan-time literal admits (statistics.md §6). A
/// parameter whose value is unavailable takes the generic equality estimate, never an MCV or
/// histogram bucket (estimator.md §6.1).
fn statistics_literal_rows(
    current: &CurrentColumnStatistics,
    op: CmpOp,
    literal: &RExpr,
    rel: &ScopeRel<'_>,
    column: usize,
    catalog: &Engine,
) -> Option<i64> {
    let param = matches!(literal, RExpr::Param(_));
    if param && let Some(rows) = statistics_value_blind_rows(current, op, rel, column) {
        return Some(rows);
    }
    if let Some(key) = statistics_literal_key(literal, rel, column, catalog) {
        return Some(statistics_comparison_rows(current, op, &key));
    }
    if !param || !matches!(op, CmpOp::Eq | CmpOp::Ne) {
        return None;
    }
    let equality = statistics_scale(current.nonnull_rows, 1, current.ndv?.max(1));
    Some(if op == CmpOp::Eq {
        equality
    } else {
        current.nonnull_rows.saturating_sub(equality)
    })
}

/// An equality (or `<>`) estimate that no non-NULL value can change (estimator.md §6.1): the facts
/// hold no MCV and do not prove absence, so every value falls in the residual population and takes
/// the NDV average — exactly the literal estimate, read without the value. This keeps a key lookup
/// on an analyzed table parameter-insensitive. A collated text column is excluded because its sort
/// key can fail for a value, which the literal path turns into the structural fallback.
fn statistics_value_blind_rows(
    current: &CurrentColumnStatistics,
    op: CmpOp,
    rel: &ScopeRel<'_>,
    column: usize,
) -> Option<i64> {
    if !matches!(op, CmpOp::Eq | CmpOp::Ne)
        || !current.mcv.is_empty()
        || current.complete_mcv
        || rel.table.columns[column].collation.is_some()
    {
        return None;
    }
    let equality = statistics_scale(current.nonnull_rows, 1, current.ndv?.max(1));
    Some(if op == CmpOp::Eq {
        equality
    } else {
        current.nonnull_rows.saturating_sub(equality)
    })
}

fn statistics_collect_equality_disjunction<'a>(
    expr: &'a RExpr,
    rel: &ScopeRel<'_>,
    column: &mut Option<usize>,
    literals: &mut Vec<&'a RExpr>,
) -> bool {
    if let RExpr::Or(lhs, rhs) = expr {
        return statistics_collect_equality_disjunction(lhs, rel, column, literals)
            && statistics_collect_equality_disjunction(rhs, rel, column, literals);
    }
    let Some((candidate, CmpOp::Eq, literal)) = statistics_comparison(expr, rel) else {
        return false;
    };
    if column.is_some_and(|existing| existing != candidate) {
        return false;
    }
    *column = Some(candidate);
    literals.push(literal);
    true
}

/// Equality disjunctions on one column are disjoint after canonical-value de-duplication. This is
/// the resolved shape of `IN`, and avoids treating its alternatives as independent OR events.
pub(crate) fn statistics_equality_disjunction_selectivity(
    expr: &RExpr,
    rel: &ScopeRel<'_>,
    catalog: &Engine,
    negated: bool,
) -> Option<crate::estimator::Selectivity> {
    let mut column = None;
    let mut literals = Vec::new();
    if !statistics_collect_equality_disjunction(expr, rel, &mut column, &mut literals) {
        return None;
    }
    let column = column?;
    let current = current_column_statistics(rel, column, catalog)?;
    let mut seen = HashSet::new();
    let mut matched = 0i64;
    let mut has_null = false;
    for literal in literals {
        if estimator_is_null(literal) {
            has_null = true;
        } else if let Some(key) = statistics_literal_key(literal, rel, column, catalog) {
            if seen.insert(key.clone()) {
                matched = matched
                    .saturating_add(statistics_equality_rows(&current, &key))
                    .min(current.nonnull_rows);
            }
        } else if matches!(literal, RExpr::Param(_)) {
            matched = matched
                .saturating_add(statistics_scale(
                    current.nonnull_rows,
                    1,
                    current.ndv?.max(1),
                ))
                .min(current.nonnull_rows);
        }
    }
    let rows = if negated {
        if has_null {
            0
        } else {
            current.nonnull_rows.saturating_sub(matched)
        }
    } else {
        matched
    };
    Some(statistics_fraction(rows, current.rows))
}

fn statistics_negated_paired_range_selectivity(
    lhs: &RExpr,
    rhs: &RExpr,
    rel: &ScopeRel<'_>,
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    let (a_column, a_op, a_literal) = statistics_comparison(lhs, rel)?;
    let (b_column, b_op, b_literal) = statistics_comparison(rhs, rel)?;
    if a_column != b_column {
        return None;
    }
    let ((lower_op, lower_literal), (upper_op, upper_literal)) =
        if matches!(a_op, CmpOp::Gt | CmpOp::Ge) {
            ((a_op, a_literal), (b_op, b_literal))
        } else {
            ((b_op, b_literal), (a_op, a_literal))
        };
    if !matches!(lower_op, CmpOp::Gt | CmpOp::Ge) || !matches!(upper_op, CmpOp::Lt | CmpOp::Le) {
        return None;
    }
    let lower_null = estimator_is_null(lower_literal);
    let upper_null = estimator_is_null(upper_literal);
    if lower_null || upper_null {
        let current = current_column_statistics(rel, a_column, catalog)?;
        return match (lower_null, upper_null) {
            (true, true) => Some(crate::estimator::Selectivity::Zero),
            (true, false) => {
                let key = statistics_literal_key(upper_literal, rel, a_column, catalog)?;
                let op = if upper_op == CmpOp::Le {
                    CmpOp::Gt
                } else {
                    CmpOp::Ge
                };
                Some(statistics_fraction(
                    statistics_comparison_rows(&current, op, &key),
                    current.rows,
                ))
            }
            (false, true) => {
                let key = statistics_literal_key(lower_literal, rel, a_column, catalog)?;
                let op = if lower_op == CmpOp::Ge {
                    CmpOp::Lt
                } else {
                    CmpOp::Le
                };
                Some(statistics_fraction(
                    statistics_comparison_rows(&current, op, &key),
                    current.rows,
                ))
            }
            (false, false) => unreachable!(),
        };
    }
    let current = current_column_statistics(rel, a_column, catalog)?;
    let positive = statistics_paired_range_selectivity(lhs, rhs, rel, catalog)?;
    let positive_rows = crate::estimator::estimate_rows(&positive, current.rows);
    Some(statistics_fraction(
        current.nonnull_rows.saturating_sub(positive_rows),
        current.rows,
    ))
}

/// SQL `NOT` preserves UNKNOWN, so supported column predicates complement within the scaled
/// non-NULL population rather than the whole table. The OR form is literal `IN` after resolution.
pub(crate) fn statistics_negated_leaf_selectivity(
    expr: &RExpr,
    rel: &ScopeRel<'_>,
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    if let Some(estimate) = statistics_equality_disjunction_selectivity(expr, rel, catalog, true) {
        return Some(estimate);
    }
    if let RExpr::And(lhs, rhs) = expr
        && let Some(estimate) = statistics_negated_paired_range_selectivity(lhs, rhs, rel, catalog)
    {
        return Some(estimate);
    }
    if let Some((column, op, literal)) = statistics_comparison(expr, rel) {
        let current = current_column_statistics(rel, column, catalog)?;
        if estimator_is_null(literal) {
            return Some(crate::estimator::Selectivity::Zero);
        }
        let rows = statistics_literal_rows(&current, op, literal, rel, column, catalog)?;
        return Some(statistics_fraction(
            current.nonnull_rows.saturating_sub(rows),
            current.rows,
        ));
    }
    if let RExpr::Column(_) = expr {
        let column = statistics_column(expr, rel)?;
        if !matches!(rel.table.columns[column].ty, Type::Scalar(ScalarType::Bool)) {
            return None;
        }
        let current = current_column_statistics(rel, column, catalog)?;
        let key = statistics_value_key(&Value::Bool(true), rel, column, catalog)?;
        return Some(statistics_fraction(
            current
                .nonnull_rows
                .saturating_sub(statistics_equality_rows(&current, &key)),
            current.rows,
        ));
    }
    None
}

/// Estimate a two-sided range once from one histogram rather than multiplying two leaves.
pub(crate) fn statistics_paired_range_selectivity(
    lhs: &RExpr,
    rhs: &RExpr,
    rel: &ScopeRel<'_>,
    catalog: &Engine,
) -> Option<crate::estimator::Selectivity> {
    let (a_column, a_op, a_literal) = statistics_comparison(lhs, rel)?;
    let (b_column, b_op, b_literal) = statistics_comparison(rhs, rel)?;
    if a_column != b_column {
        return None;
    }
    let ((lower_op, lower_literal), (upper_op, upper_literal)) =
        if matches!(a_op, CmpOp::Gt | CmpOp::Ge) {
            ((a_op, a_literal), (b_op, b_literal))
        } else {
            ((b_op, b_literal), (a_op, a_literal))
        };
    if !matches!(lower_op, CmpOp::Gt | CmpOp::Ge) || !matches!(upper_op, CmpOp::Lt | CmpOp::Le) {
        return None;
    }
    let lower = statistics_literal_key(lower_literal, rel, a_column, catalog)?;
    let upper = statistics_literal_key(upper_literal, rel, a_column, catalog)?;
    let current = current_column_statistics(rel, a_column, catalog)?;
    let mcv_rows = current.mcv.iter().fold(0i64, |sum, (key, rows)| {
        if statistics_key_satisfies((*key).cmp(&lower), lower_op)
            && statistics_key_satisfies((*key).cmp(&upper), upper_op)
        {
            sum.saturating_add(*rows)
        } else {
            sum
        }
    });
    let all_mcv_rows = current
        .mcv
        .iter()
        .fold(0i64, |sum, (_, rows)| sum.saturating_add(*rows))
        .min(current.nonnull_rows);
    let residual = current.nonnull_rows - all_mcv_rows;
    let histogram_rows = if current.histogram.len() >= 2 {
        let lower_ordinal = current.histogram.partition_point(|bound| {
            (*bound) < lower.as_slice() || (lower_op == CmpOp::Gt && (*bound) == lower.as_slice())
        });
        let upper_ordinal = current.histogram.partition_point(|bound| {
            (*bound) < upper.as_slice() || (upper_op == CmpOp::Le && (*bound) == upper.as_slice())
        });
        statistics_scale(
            residual,
            upper_ordinal.saturating_sub(lower_ordinal) as i64,
            current.histogram.len() as i64 - 1,
        )
        .min(residual)
    } else {
        statistics_scale(
            residual,
            SELECTIVITY_PAIRED_RANGE.numerator,
            SELECTIVITY_PAIRED_RANGE.denominator,
        )
    };
    Some(statistics_fraction(
        mcv_rows
            .saturating_add(histogram_rows)
            .min(current.nonnull_rows),
        current.rows,
    ))
}

impl Engine {
    pub(crate) fn execute_analyze(&mut self, analyze: Analyze) -> Result<Outcome> {
        self.check_attachment_writable(analyze.db.as_deref())?;
        check_catalog_rel_write(&analyze.name)?;
        // ANALYZE of a view is a no-op success (views.md §2; PostgreSQL also warns).
        if self
            .table_scoped(analyze.db.as_deref(), &analyze.name)
            .is_none()
            && matches!(analyze.db.as_deref(), None | Some("main"))
            && self.is_view(&analyze.name)
        {
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }
        let table = self
            .table_scoped(analyze.db.as_deref(), &analyze.name)
            .cloned()
            .ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("table does not exist: {}", analyze.name),
                )
            })?;

        let mut columns = Vec::new();
        let mut seen = HashSet::new();
        if analyze.columns.is_empty() {
            columns.extend(0..table.columns.len());
        } else {
            for name in &analyze.columns {
                let key = name.to_string();
                if !seen.insert(key.clone()) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {name} appears more than once"),
                    ));
                }
                let column = table
                    .columns
                    .iter()
                    .position(|column| column.name == *name)
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column does not exist: {name}"),
                        )
                    })?;
                columns.push(column);
            }
        }

        let store = self
            .store_scoped(analyze.db.as_deref(), &analyze.name)
            .clone();
        let colls =
            self.column_collations_scoped(analyze.db.as_deref(), &table.name, &table.columns);
        let mut meter = self.session.new_meter();
        let mut facts = Vec::with_capacity(columns.len());
        for &column in &columns {
            meter.charge(COSTS.page_read * store.node_count() as i64);
            let eligible = distribution_eligible(&table.columns[column].ty);
            let term_opclass = term_eligible(&table.columns[column].ty)
                .then(|| GinOpclass::for_type(&table.columns[column].ty))
                .flatten();
            let mut term_counter = TermCounter::default();
            let mut scan = store.store_scan(KeyBound::unbounded(), false);
            let mut sample = BinaryHeap::new();
            let mut kmv = BinaryHeap::new();
            let mut kmv_seen = HashSet::new();
            let mut analyzed_rows = 0i64;
            let mut null_count = 0i64;
            let mut width_sum = 0i64;
            let mask: Vec<bool> = (0..table.columns.len()).map(|i| i == column).collect();
            while let Some((storage_key, mut row)) = scan.next()? {
                meter.checkpoint()?;
                let units = store.statistics_scan_units(&storage_key, &row, column);
                meter.charge(
                    COSTS.page_read * units.pages as i64
                        + COSTS.value_decompress * units.decompress as i64
                        + COSTS.storage_row_read,
                );
                scan.resolve_columns(&mut row, &mask)?;
                let value = &row[column];
                let priority = fnv1a64(&storage_key);
                let ordinal = analyzed_rows as u64;
                analyzed_rows = analyzed_rows.saturating_add(1);
                if matches!(value, Value::Null) {
                    null_count = null_count.saturating_add(1);
                    meter.charge(COSTS.statistics_value);
                    retain_lowest_sample(
                        &mut sample,
                        SampleRow {
                            priority,
                            ordinal,
                            nonnull: false,
                            oversized: false,
                            retained: None,
                        },
                    );
                    continue;
                }

                let encoded = crate::format::encode_value(store.column_type(column), value);
                let body_len = encoded.len().saturating_sub(1);
                let key = if eligible {
                    Some(encode_typed_key(
                        &table.columns[column].ty,
                        value,
                        colls[column].as_deref(),
                    )?)
                } else {
                    None
                };
                let width = key.as_ref().map_or(body_len, Vec::len);
                width_sum = width_sum.saturating_add(i64::try_from(width).unwrap_or(i64::MAX));
                meter.charge(
                    COSTS.statistics_value * i64::try_from(width.max(1)).unwrap_or(i64::MAX),
                );
                if let Some(key) = &key {
                    retain_kmv(&mut kmv, &mut kmv_seen, fnv1a64(key));
                }
                if let Some(opclass) = term_opclass {
                    for term in gin_value_terms(opclass, value) {
                        meter.charge(
                            COSTS.statistics_value
                                * i64::try_from(term.len().max(1)).unwrap_or(i64::MAX),
                        );
                        term_counter.add(&term);
                    }
                }
                let oversized = body_len > STATISTICS_MAX_VALUE_BYTES
                    || key
                        .as_ref()
                        .is_some_and(|key| key.len() > STATISTICS_MAX_VALUE_BYTES);
                retain_lowest_sample(
                    &mut sample,
                    SampleRow {
                        priority,
                        ordinal,
                        nonnull: true,
                        oversized,
                        retained: if !oversized {
                            key.map(|key| StatisticsValue {
                                value: value.clone(),
                                key,
                            })
                        } else {
                            None
                        },
                    },
                );
            }
            meter.checkpoint()?;
            let nonnull_rows = analyzed_rows.saturating_sub(null_count);
            let distinct_count = eligible.then(|| kmv_count(&kmv, nonnull_rows));
            let sample_rows = sample.len() as u32;
            let (sample_nonnull_rows, mcv, histogram) = if let Some(ndv) = distinct_count {
                finish_distribution(sample.into_vec(), analyzed_rows, ndv)
            } else {
                (
                    sample.iter().filter(|row| row.nonnull).count() as u32,
                    Vec::new(),
                    Vec::new(),
                )
            };
            facts.push((
                column,
                ColumnStatistics {
                    analyzed_rows,
                    stale: false,
                    null_count,
                    width_sum,
                    distinct_count,
                    sample_rows,
                    sample_nonnull_rows,
                    mcv,
                    histogram,
                    terms: term_opclass.map(|_| term_counter.finish()),
                },
            ));
        }

        let database = match analyze.db.as_deref() {
            None if self.is_temp_table(&analyze.name) => "temp".to_string(),
            None => "main".to_string(),
            Some(scope) => scope.to_string(),
        };
        let target = match database.as_str() {
            "temp" => self.temp_working_mut(),
            "main" => self.working_mut(),
            attachment => self.attach_write_snap(attachment),
        };
        for (column, statistics) in facts {
            target.put_column_statistics(&table.name, column, statistics);
        }
        if database != "temp" {
            target.bump_estimator_revision(&table.name);
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
            rows_affected: Some(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::statistics_local_column;

    #[test]
    fn statistics_column_rejects_columns_from_earlier_join_relations() {
        assert_eq!(statistics_local_column(1, 2, 3), None);
        assert_eq!(statistics_local_column(2, 2, 3), Some(0));
        assert_eq!(statistics_local_column(4, 2, 3), Some(2));
        assert_eq!(statistics_local_column(5, 2, 3), None);
    }
}
