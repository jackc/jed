//! The aggregate and window plan IR — `AggCtx`, `WindowSpec`, `AggPlan`, `AggSpec` and the
//! accumulator `Acc` — and the finalizers of the ordered-set, percentile and hypothetical-set
//! aggregates (aggregates.md). The fold loops that drive them live in `aggregate.rs` and
//! `exec_emit.rs`; the window driver in `window.rs`.
//!
//! An aggregate query's select list resolves in `Collect` mode: each aggregate call is
//! collected into an `AggSpec` (its plan + resolved argument) and replaced by a reference to
//! a synthetic-row slot (an `RExpr::Column(slot)` indexing the finalized aggregate results),
//! so the existing evaluator projects the result with no new node. Outside Collect mode
//! (`Forbidden`: WHERE / ON / an aggregate's own argument / any non-aggregate query) a column
//! resolves normally and an aggregate call is a 42803 grouping error.

use super::*;

/// A general-expression `GROUP BY` key (aggregates.md §15): its canonical AST, its resolved type, and
/// — when the key is a bare enclosing-query column (query.group_by_correlated) — that column's
/// `(level, index)`, so every spelling of the same outer column (`x`, `o.x`) matches it, as PostgreSQL
/// matches grouping expressions by the resolved column.
pub(crate) type GroupKeyExpr = (Expr, ResolvedType, Option<(usize, usize)>);

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
        group_key_exprs: Vec<Option<GroupKeyExpr>>,
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
        group_key_exprs: Vec<Option<GroupKeyExpr>>,
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
    pub(crate) order: Vec<SortKey>,
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
    /// A text `MIN`/`MAX` window aggregate's comparison collation (see [`AggSpec::collation`]).
    pub(crate) agg_collation: Option<std::sync::Arc<Collation>>,
    /// A JSON window aggregate's operand type (see [`AggSpec::json_tys`]).
    pub(crate) agg_json_tys: Vec<ResolvedType>,
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

/// Which statistic a one-argument statistical aggregate finalizes (aggregates.md §23).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MomentStat {
    StddevSamp,
    StddevPop,
    VarPop,
    VarSamp,
}

impl MomentStat {
    /// The statistic a one-argument statistical aggregate's surface names.
    pub(crate) fn from_surface(surface: &str) -> Option<MomentStat> {
        Some(match surface {
            "var_pop" => MomentStat::VarPop,
            "stddev_samp" | "stddev" => MomentStat::StddevSamp,
            "stddev_pop" => MomentStat::StddevPop,
            "var_samp" | "variance" => MomentStat::VarSamp,
            _ => return None,
        })
    }

    /// Whether the statistic is a sample (N − 1) rather than a population (N) one, and whether it
    /// is a standard deviation rather than a variance.
    pub(crate) fn sample_stddev(self) -> (bool, bool) {
        match self {
            MomentStat::VarPop => (false, false),
            MomentStat::StddevSamp => (true, true),
            MomentStat::StddevPop => (false, true),
            MomentStat::VarSamp => (true, false),
        }
    }
}

/// Which statistic a two-argument statistical aggregate finalizes (aggregates.md §23).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairStat {
    CovarPop,
    RegrR2,
    RegrIntercept,
    RegrSlope,
    RegrSxy,
    RegrSyy,
    RegrSxx,
    RegrAvgy,
    RegrAvgx,
    Corr,
    CovarSamp,
}

impl PairStat {
    /// The statistic a two-argument statistical aggregate's surface names.
    pub(crate) fn from_surface(surface: &str) -> Option<PairStat> {
        Some(match surface {
            "covar_pop" => PairStat::CovarPop,
            "regr_r2" => PairStat::RegrR2,
            "regr_intercept" => PairStat::RegrIntercept,
            "regr_slope" => PairStat::RegrSlope,
            "regr_sxy" => PairStat::RegrSxy,
            "regr_syy" => PairStat::RegrSyy,
            "regr_sxx" => PairStat::RegrSxx,
            "regr_avgy" => PairStat::RegrAvgy,
            "regr_avgx" => PairStat::RegrAvgx,
            "corr" => PairStat::Corr,
            "covar_samp" => PairStat::CovarSamp,
            _ => return None,
        })
    }
}

/// The bitwise operation a bit aggregate folds with (aggregates.md §23).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitOp {
    And,
    Or,
    Xor,
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
    /// bool_and(x) — TRUE iff every non-NULL input is TRUE (aggregates.md §23).
    BoolAnd,
    /// bool_or(x) — TRUE iff some non-NULL input is TRUE (aggregates.md §23).
    BoolOr,
    /// bit_and / bit_or / bit_xor(x) — the bitwise fold of the non-NULL integer inputs, result the
    /// input type (aggregates.md §23).
    Bit(BitOp),
    /// array_agg(x) — the inputs, NULLs included, in fold order (aggregates.md §23): a 1-D array, or
    /// over an array input (`nested`) the inputs stacked into one more dimension.
    ArrayAgg {
        nested: bool,
    },
    /// string_agg(value, delimiter) — the non-NULL values joined in fold order, each after the first
    /// preceded by its own row's delimiter (aggregates.md §23). The operand is a 2-field Row.
    StringAgg,
    /// The one-argument statistical aggregates (aggregates.md §23): exact over integer/decimal input
    /// (decimal result), PostgreSQL's float8 algorithm over float input (`float`, f64 result).
    Moment {
        stat: MomentStat,
        float: bool,
    },
    /// regr_count(Y, X) — the number of pairs whose arguments are both non-NULL (aggregates.md §23).
    RegrCount,
    /// The two-argument statistical aggregates over (Y, X) (aggregates.md §23): numeric arguments
    /// widened to f64, an f64 result.
    Pair {
        stat: PairStat,
    },
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

impl AggPlan {
    /// Whether a DISTINCT aggregate keeps a NULL argument as one more distinct value instead of
    /// skipping it (aggregates.md §5): only the JSON array aggregates, whose non-strict transition
    /// folds a NULL (even the `_strict` variants, which then drop it in the fold but still mark the
    /// group non-empty — `[]`, not NULL, as in PostgreSQL). Every other aggregate ignores NULLs.
    pub(crate) fn distinct_keeps_null(self) -> bool {
        matches!(self, AggPlan::JsonAgg { .. } | AggPlan::ArrayAgg { .. })
    }
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
    /// `Some` for an aggregate with an in-aggregate `ORDER BY` (`json_agg(x ORDER BY y)` —
    /// aggregates.md §22): the per-row sort keys + their sort specs. The fold loop buffers each
    /// passing row's `[sort keys…, argument]` and folds the arguments in key order at finalize.
    pub(crate) order: Option<AggOrder>,
    /// The comparison collation of a text `MIN`/`MAX` (spec/design/collation.md §1 "Aggregates and
    /// window functions"): the operand's derivation; `None` for `C`, a non-text operand, or any
    /// other aggregate.
    pub(crate) collation: Option<std::sync::Arc<Collation>>,
    /// A JSON aggregate's operand type(s) (see [`JsonAggConv`]); empty for every other aggregate.
    pub(crate) json_tys: Vec<ResolvedType>,
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

/// An in-aggregate `ORDER BY` (aggregates.md §22): `keys` are the sort-key expressions (evaluated
/// per passing input row, after the argument) and `sorts` their per-key direction / NULL placement /
/// collation. Equal length.
pub(crate) struct AggOrder {
    pub(crate) keys: Vec<RExpr>,
    pub(crate) sorts: Vec<KeySort>,
}

/// A running aggregate accumulator (one per AggSpec), folded per input row then finalized.
/// What a JSON aggregate converts its inputs with (json-sql-functions.md §4): the operand's static
/// type(s) — `[value]` for json[b]_agg, `[key, value]` for json[b]_object_agg — and the session zone
/// a `timestamptz` renders in. Empty types for every other aggregate.
#[derive(Clone)]
pub(crate) struct JsonAggConv {
    pub(crate) tys: Vec<ResolvedType>,
    pub(crate) zone: crate::timezone::ZoneRef,
}

/// `Clone` so the window stage can snapshot a running accumulator at each peer-group boundary
/// (a running aggregate window's default frame — spec/design/window.md §6) without consuming it.
#[derive(Clone)]
pub(crate) enum Acc {
    CountStar(i64),
    Count(i64),
    /// regr_count: the number of (Y, X) pairs with both arguments non-NULL.
    RegrCount(i64),
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
    /// bool_and / bool_or: the running conjunction (`is_and`) or disjunction of the non-NULL inputs,
    /// `None` until one is folded (aggregates.md §23).
    Bool {
        cur: Option<bool>,
        is_and: bool,
    },
    /// bit_and / bit_or / bit_xor: the running bitwise fold of the non-NULL inputs, `None` until one
    /// is folded (aggregates.md §23).
    Bit {
        cur: Option<i64>,
        op: BitOp,
    },
    /// array_agg: the inputs in fold order, NULLs included; `seen` marks the group non-empty. For an
    /// array input (`nested`), `shape` is the first input's `(dims, lbounds)`, which every later
    /// input must match (`None` before the first).
    ArrayAgg {
        elems: Vec<Value>,
        seen: bool,
        nested: bool,
        shape: Option<(Vec<usize>, Vec<i32>)>,
    },
    /// string_agg: the `[value, delimiter]` of every row with a non-NULL value, in fold order; `seen`
    /// marks that one was folded.
    StringAgg {
        rows: Vec<Vec<Value>>,
        seen: bool,
    },
    /// The exact one-argument moments (aggregates.md §23): the count of non-NULL inputs and the
    /// decimal sums of x and x².
    MomentsExact {
        stat: MomentStat,
        n: i64,
        sx: Decimal,
        sxx: Decimal,
    },
    /// PostgreSQL's float8_accum transition state: N, Σx, and Sxx (the sum of squared deviations
    /// from the running mean), all f64.
    MomentsFloat {
        stat: MomentStat,
        n: f64,
        sx: f64,
        sxx: f64,
    },
    /// PostgreSQL's float8_regr_accum transition state: N, Σx, Sxx, Σy, Syy, Sxy, all f64.
    PairsFloat {
        stat: PairStat,
        t: [f64; 6],
    },
    MinMax {
        cur: Option<Value>,
        is_min: bool,
        /// A text / text[] MIN/MAX's comparison collation (spec/design/collation.md §1); `None` is
        /// `C`.
        collation: Option<std::sync::Arc<Collation>>,
    },
    /// json_agg / jsonb_agg accumulator (B4): the inputs' JSON images in row order — a `Value::Json`
    /// text (a NULL input `Value::Null`) for json_agg, a `Value::Jsonb` node for jsonb_agg. `compact`
    /// selects the json vs jsonb result; `strict` skips NULL inputs. `seen` records whether the group
    /// had ANY input row: a zero-row group → SQL NULL, but a non-empty group all of whose rows the
    /// strict filter dropped → an empty array `[]` (PG distinguishes the two). `conv` converts each
    /// input as it is folded.
    JsonAgg {
        elems: Vec<Value>,
        compact: bool,
        strict: bool,
        seen: bool,
        conv: JsonAggConv,
    },
    /// json_object_agg / jsonb_object_agg accumulator (B4): the (key text, value image) pairs in row
    /// order — the value a `Value::Json` text (`Value::Null` for NULL) or a `Value::Jsonb` node, as
    /// for [`Acc::JsonAgg`]. `json` selects the json vs jsonb finalize render; `unique` errors `22030`
    /// on a duplicate key. `seen` distinguishes a zero-row group (→ NULL) from a non-empty one (→ an
    /// object, maybe `{}`).
    JsonObjectAgg {
        pairs: Vec<(String, Value)>,
        json: bool,
        unique: bool,
        seen: bool,
        conv: JsonAggConv,
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
    /// An aggregate with an in-aggregate `ORDER BY` (aggregates.md §22): every passing input row's
    /// `[sort key…, argument]` tuple (collated text keys already replaced by their UCA sort key), in
    /// input order. Finalize stable-sorts the tuples by the spec's `AggOrder` and folds the arguments
    /// into a fresh unordered accumulator — the fold runs in key order, ties in input order.
    Ordered {
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
    mut meter: Option<&mut Meter>,
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
            sort_osa_vals(&mut vals, collation, desc, meter.as_deref_mut())?;
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
            sort_osa_vals(&mut vals, collation, desc, meter.as_deref_mut())?;
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
            sort_osa_vals(&mut vals, collation, desc, meter)?;
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
            Ok(Value::Array(crate::value::ArrayVal {
                dims: arr.dims.clone(),
                lbounds: arr.lbounds.clone(),
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
            let base = collated_or_value_cmp(ks.collation.as_deref(), a, b)?;
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
///
/// With `meter`, the decorations are transient operator state (memory.md §6.8): each reserves its
/// sort key's `bytea` value bytes as it is built, all released once the values are undecorated.
pub(crate) fn sort_osa_vals(
    vals: &mut Vec<Value>,
    collation: Option<&Collation>,
    desc: bool,
    meter: Option<&mut Meter>,
) -> Result<()> {
    match collation {
        None => {
            vals.sort_by(|a, b| dir_cmp(value_cmp(a, b), desc));
            Ok(())
        }
        Some(c) => {
            let mut meter = meter;
            let mut charge = match meter.as_deref() {
                Some(m) => m.state_charge(),
                None => crate::cost::StateCharge::default(),
            };
            let mut decorated: Vec<(Vec<u8>, Value)> = Vec::with_capacity(vals.len());
            for v in vals.drain(..) {
                let key = match collation_sort_key(c, &v)? {
                    Some(k) => k,
                    None => unreachable!(
                        "a collated WITHIN GROUP key buffers only text or text[]: {v:?}"
                    ),
                };
                if let Some(m) = meter.as_deref_mut()
                    && charge.active()
                {
                    charge.reserve(m, crate::costs::MEMORY_VALUE + key.len() as i64)?;
                }
                decorated.push((key, v));
            }
            decorated.sort_by(|a, b| dir_cmp(a.0.cmp(&b.0), desc));
            vals.extend(decorated.into_iter().map(|(_, v)| v));
            charge.release_all();
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
