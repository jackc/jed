//! The resolver's name scope (grammar.md §15): the relations a FROM clause puts in scope and how
//! a column reference resolves against them, plus the CTE bindings a `WITH` introduces.

use super::*;

// ============================================================================
// Resolution scope (multi-table FROM — spec/design/grammar.md §15).
//
// A `Scope` is the ordered list of relations a SELECT's FROM clause puts in scope, each
// carrying the flat COLUMN OFFSET at which its columns begin in the concatenated (joined)
// row. A resolved column reference bakes a single flat index `offset + local` into
// `RExpr::Column`, so the joined row is just each relation's row concatenated in FROM order
// and the expression evaluator is unchanged. A single-table SELECT / UPDATE / DELETE is a
// one-relation scope (offset 0), so the same resolver serves every statement.
//
// NOTE (forward-compat): the scope keys resolution ONLY on column name and type — never on a
// column's NOT NULL / PRIMARY KEY flags. A column on the nullable side of a future outer join
// is NULL-extended at runtime regardless of its declared nullability, so no resolver shortcut
// may fold on it (spec/design/grammar.md §15).
// ============================================================================

/// One relation in a FROM scope: its label (alias, else table name — exact for
/// byte-exact matching), the table, and the flat offset of its first column in the
/// joined row. A `qualifier_only` relation is visible ONLY to qualified references — the
/// RETURNING `old`/`new` row-version pseudo-relations (grammar.md §32): bare-column
/// resolution skips it (no new ambiguity), every other statement never builds one.
#[derive(Clone)]
pub(crate) struct ScopeRel<'a> {
    pub(crate) label: String,
    pub(crate) table: &'a Table,
    pub(crate) offset: usize,
    pub(crate) qualifier_only: bool,
    /// Whether this relation is a catalog **base table** (possibly under a column alias list —
    /// grammar.md §15). A view body's planning trace records the columns resolved against such a
    /// relation as dependencies (spec/design/views.md §6); every other relation kind records none.
    pub(crate) base: bool,
    /// The view this relation is a reference to (spec/design/views.md §4), else `None`. A traced
    /// view body records the columns resolved against it, so re-validation after `CREATE OR REPLACE
    /// VIEW` can tell whether a dependent binds a column the replacement appended (§8).
    pub(crate) view: Option<String>,
    /// `Some(i)` when this relation is a reference to CTE `i` (spec/design/cte.md) rather than a
    /// base table — its `table` is the binding's synthetic relation and exec delivers its rows from
    /// the `CteCtx`. `None` for a base table / SRF / pseudo-relation.
    pub(crate) cte: Option<usize>,
    /// The relation's explicit database qualifier (attached-databases.md §3), carried from the
    /// `TableRef` so the store is re-looked-up in the right database at exec (a store is resolved by
    /// name per-access). `None` for a bare (implicit-scope) name — then the scoped funnels fall through
    /// to the temp-first walk, so this is behavior-neutral for every unqualified query.
    pub(crate) db: Option<String>,
    /// The target of a write through a view (spec/design/views.md §15.2): the names this relation
    /// exposes are the layer's columns, not `table`'s, and each resolves through the layer to a
    /// base column or a read-only select item. `table` stays the base table, so flat indices keep
    /// the base row's layout. `None` for every other relation.
    pub(crate) layer: Option<&'a WriteLayer>,
}

impl ScopeRel<'_> {
    /// Whether this relation targets a HOST-ATTACHED database (attached-databases.md §3) rather than
    /// the implicit `main`/`temp` scope. Index/PK/GiST/GIN bound pushdown is gated OFF for an attachment
    /// relation this slice — the bounded-scan exec path resolves index stores through the UNSCOPED
    /// funnel, so an attachment relation must full-scan (correct, perf-only — index acceleration for
    /// attachments is a Slice 1b perf follow-on). A full scan reads the scoped store.
    pub(crate) fn is_attachment(&self) -> bool {
        is_attachment_scope(self.db.as_deref())
    }
}

/// Whether a database qualifier names one of the two implicit reserved scopes `main` / `temp`
/// (attached-databases.md §3), which resolve to the SAME store the bare name would. A `None` qualifier
/// (a bare implicit-scope name) counts as reserved for routing: it too keeps the temp-first funnels.
/// Reject a USER-written catalog object name beginning `jed_` — the prefix is reserved for the
/// engine's own catalog relations (spec/design/introspection.md §4). Byte-exact on the (folded)
/// name: a bare `JED_x` folds to `jed_x` and is rejected, a quoted `"JED_x"` is a different name and
/// is allowed (grammar.md §3, PostgreSQL's `pg_` rule). Engine-GENERATED names (a
/// serial's `<table>_<col>_seq`, an index auto-name — both legal for a table named `jed`) never
/// pass through here; the check sits with each site's namespace-collision check so established
/// validation orders (42P01/42703 before name checks) are preserved. `kind` is the object word in
/// the message: table / index / sequence / type.
pub(crate) fn check_reserved_name(kind: &str, name: &str) -> Result<()> {
    if name.starts_with("jed_") {
        return Err(EngineError::new(
            SqlState::ReservedName,
            format!(
                "{kind} name {name} is reserved (the jed_ prefix is reserved for system objects)"
            ),
        ));
    }
    Ok(())
}

pub(crate) fn is_reserved_scope(q: Option<&str>) -> bool {
    match q {
        None => true,
        Some(s) => matches!(s, "main" | "temp"),
    }
}

/// Whether a database qualifier names a HOST-ATTACHED database (not `None`, not reserved `main`/`temp`)
/// — the case that routes to the attachment registry and gates off index-bound pushdown this slice.
pub(crate) fn is_attachment_scope(q: Option<&str>) -> bool {
    !is_reserved_scope(q)
}

/// Where a finalized FROM relation's `&Table` comes from, recorded during the LATERAL-aware FROM
/// build (spec/design/grammar.md §44). A base table / CTE binding has a stable catalog address
/// (`&Table`); a synthetic relation (derived table / SRF) is recorded by INDEX into the local
/// `synthetic` vec — never a borrow — so a record can outlive a later push into that vec, which is
/// what lets a LATERAL item resolve against the earlier synthetic tables while later ones grow.
#[derive(Clone, Copy)]
pub(crate) enum RelSrc<'a> {
    Base(&'a Table),
    Cte(&'a Table, usize),
    Synthetic(usize),
    /// A base table under a column alias list (grammar.md §15): a relabeled copy of the catalog
    /// table in the synthetic vec, still read from the table's own store — the rename is a pure
    /// resolution-time relabeling.
    BaseRenamed(usize),
    /// A CTE reference under a column alias list: a relabeled copy of the binding's relation.
    CteRenamed(usize, usize),
}

impl RelSrc<'_> {
    /// Whether the relation is a catalog base table (renamed or not).
    pub(crate) fn is_base(&self) -> bool {
        matches!(self, RelSrc::Base(_) | RelSrc::BaseRenamed(_))
    }

    /// The CTE binding index of a CTE reference (renamed or not).
    pub(crate) fn cte(&self) -> Option<usize> {
        match self {
            RelSrc::Cte(_, ci) | RelSrc::CteRenamed(_, ci) => Some(*ci),
            _ => None,
        }
    }
}

/// A FROM relation finalized during the §44 LATERAL-aware build: its label, flat column offset, and
/// table source. Held in FROM order so the prefix `parent` scope a later LATERAL item resolves
/// against (the relations to its left) can be rebuilt, and the persistent scope assembled afterward.
pub(crate) struct FinalRel<'a> {
    pub(crate) label: String,
    pub(crate) offset: usize,
    pub(crate) src: RelSrc<'a>,
    /// The relation's explicit database qualifier (attached-databases.md §3), carried from the
    /// `TableRef` into the `ScopeRel`/`PlanRel` so the store routes to the right database at exec.
    /// `None` for a bare (implicit-scope) name / a synthetic relation.
    pub(crate) db: Option<String>,
    /// The view this FROM item references, carried into the `ScopeRel` (views.md §8).
    pub(crate) view: Option<String>,
}

impl<'a> FinalRel<'a> {
    /// The relation's `&Table` — a borrowed catalog table, or a deref into the synthetic-table vec.
    pub(crate) fn table<'s>(&'s self, synthetic: &'s [Box<Table>]) -> &'s Table
    where
        'a: 's,
    {
        match self.src {
            RelSrc::Base(t) | RelSrc::Cte(t, _) => t,
            RelSrc::Synthetic(idx) | RelSrc::BaseRenamed(idx) | RelSrc::CteRenamed(idx, _) => {
                &synthetic[idx]
            }
        }
    }
}

/// Build the temporary `parent` scope a LATERAL item resolves against (spec/design/grammar.md §44):
/// the relations to its left, chained to the enclosing query's `parent` so a sibling column resolves
/// as `Outer{level=1}` and an enclosing-query column as a deeper hop. `lateral_refused` marks them
/// the left side of a `RIGHT`/`FULL` join, which the item may not reference (`42P10`);
/// `shares_parent_level` marks `parent` as a fence's view in the same FROM namespace (§15). The returned scope borrows
/// `synthetic`; it is dropped before the next push, so it never blocks the build's growth.
pub(crate) fn build_prefix_scope<'s>(
    finalized: &'s [FinalRel<'s>],
    synthetic: &'s [Box<Table>],
    group_subs: &'s [GroupSubRel],
    parent: Option<&'s Scope<'s>>,
    catalog: &'s Engine,
    ctes: &'s [&'s CteBinding],
    lateral_refused: bool,
    shares_parent_level: bool,
) -> Scope<'s> {
    // The inner labels of the unaliased parenthesized joins to its left (grammar.md §15).
    let sub_rels = sub_rels_within(group_subs, 0, usize::MAX);
    Scope {
        rels: finalized
            .iter()
            .map(|fr| ScopeRel {
                label: fr.label.clone(),
                table: fr.table(synthetic),
                offset: fr.offset,
                qualifier_only: false,
                view: fr.view.clone(),
                base: fr.src.is_base(),
                // The prefix is only for column resolution; a correlated reference into a CTE-backed
                // relation reads its already-delivered row, so it adds no CTE reference here.
                cte: None,
                db: fr.db.clone(),
                layer: None,
            })
            .collect(),
        parent,
        catalog,
        allow_subquery: true,
        ctes,
        merges: Vec::new(),
        hidden: Vec::new(),
        sub_rels,
        star: None,
        storage_collation: false,
        subquery_derivs: Default::default(),
        lateral_view: None,
        lateral_hit: Default::default(),
        grouped_keys: Default::default(),
        lateral_refused,
        shares_parent_level,
        merge_action: None,
        hints: Default::default(),
    }
}

impl<'a> Scope<'a> {
    /// The lateral fence a parenthesized join's body is planned against (spec/design/grammar.md
    /// §15): an empty level over `parent`, so the body's ON clauses and non-LATERAL items see no
    /// relation to the join's left, carrying `view` — the prefix of those relations, one scope at the
    /// same correlation level — for the body's LATERAL items and table functions.
    pub(crate) fn lateral_fence(
        parent: Option<&'a Scope<'a>>,
        view: &'a Scope<'a>,
        catalog: &'a Engine,
        ctes: &'a [&'a CteBinding],
    ) -> Scope<'a> {
        let mut fence = Scope::empty_level(parent, catalog, ctes);
        fence.lateral_view = Some(view);
        fence
    }

    /// This fence without its lateral view: the same empty level, read by a SELECT that is not the
    /// parenthesized join the fence was built for (§15).
    pub(crate) fn without_lateral_view(&self) -> Scope<'a> {
        Scope::empty_level(self.parent, self.catalog, self.ctes)
    }

    /// A correlation level with no relations of its own.
    pub(crate) fn empty_level(
        parent: Option<&'a Scope<'a>>,
        catalog: &'a Engine,
        ctes: &'a [&'a CteBinding],
    ) -> Scope<'a> {
        Scope {
            rels: Vec::new(),
            parent,
            catalog,
            allow_subquery: true,
            ctes,
            merges: Vec::new(),
            hidden: Vec::new(),
            sub_rels: Vec::new(),
            star: None,
            storage_collation: false,
            subquery_derivs: Default::default(),
            lateral_view: None,
            lateral_hit: Default::default(),
            grouped_keys: Default::default(),
            lateral_refused: false,
            shares_parent_level: false,
            merge_action: None,
            hints: Default::default(),
        }
    }
}

/// A planned common table expression, owned by `plan_with` for the whole statement (so the scopes
/// that borrow its synthetic `table` outlive it — spec/design/cte.md §A.2). `name` is exact
/// for byte-exact FROM matching; `table` is the synthetic relation exposing the body's output
/// columns; `source` is the planned body (a query plan, or — spec/design/writable-cte.md — a
/// data-modifying statement); `hint` is the `[NOT] MATERIALIZED` override; `refs` counts the FROM
/// references resolved to it during planning (a `Cell` — planning borrows `&self`).
///
/// For a RECURSIVE CTE (spec/design/recursive-cte.md) `source` holds the **non-recursive (anchor)
/// term** (its column types fix the synthetic relation's) and `recursive` carries the recursive
/// term + the `UNION ALL` flag; the binding is in scope inside its own recursive term, so the
/// self-reference resolves to it (`refs` then counts the self-reference too).
///
/// A CTE **inlined at its reference** (`at_reference`, spec/design/planner.md §3.4) — a
/// non-recursive, uncorrelated query CTE, not `MATERIALIZED`, whose statement names it exactly once
/// — keeps its body syntax (`body_ast`): its one FROM reference plans the body again, there, like a
/// derived table, so stage 2 can push conjuncts into it. `source` is then the body planned without
/// any such expansion (its own references to other such CTEs stay unexpanded), which runs only if
/// the reference was never expanded. `expanded` records that the reference was: a binding is
/// expanded at most once per statement, and an expanded binding is always inlined.
pub(crate) struct CteBinding {
    pub(crate) name: String,
    pub(crate) table: Box<Table>,
    pub(crate) source: CteSource,
    pub(crate) recursive: Option<RecursiveTerm>,
    pub(crate) hint: Option<bool>,
    pub(crate) refs: std::cell::Cell<usize>,
    pub(crate) at_reference: bool,
    pub(crate) body_ast: Option<QueryExpr>,
    pub(crate) expanded: std::cell::Cell<bool>,
    /// How many trailing columns `*` and `rel.*` skip: a recursive CTE's `SEARCH` / `CYCLE` columns
    /// while its own recursive term is planned, so `SELECT * FROM self` keeps the anchor's arity
    /// (recursive-cte.md §9). Zero otherwise.
    pub(crate) star_hidden: std::cell::Cell<usize>,
}

/// The query that owns a `WITH` list — its main body, which (with the later CTE bodies) holds the
/// static references that decide whether a CTE is inlined at its reference (planner.md §3.4).
#[derive(Clone, Copy)]
pub(crate) enum CtePrimary<'q> {
    Body(&'q CteBody),
    Query(&'q QueryExpr),
}

/// What the `cte_refs_*` walkers do at each plain relation reference they reach: it receives the
/// referenced name and returns that reference's contribution to the walk's count. Two visitors
/// exist — the single-name counter behind [`count_self_refs_query`] / [`count_cte_refs_dml`], and
/// the per-name tally behind [`CteRefTally`], which the WITH planner uses so a list of n CTEs is
/// counted in O(total size), not O(n × size) — one walk per body, not one per (body, name). The
/// `shadowed` slice carries the names a nested WITH's inner CTEs shadow for its later bodies and its
/// main query (cte.md §7): a reference to such a name there is not a reference to the outer CTE.
pub(crate) type CteRefVisitor<'a> = dyn FnMut(&str) -> usize + 'a;

/// Total self-references to `name` anywhere in a query expression (deep — FROM relations at every
/// nesting level plus expression sublinks).
pub(crate) fn count_self_refs_query(qe: &QueryExpr, name: &str) -> usize {
    cte_refs_query(qe, &[], &mut |r| usize::from(r == name))
}

/// References to CTE `name` reachable through a `cte_body`'s inner queries — the writable-CTE
/// analogue of [`count_self_refs_query`] (spec/design/writable-cte.md §3).
pub(crate) fn count_cte_refs_dml(body: &CteBody, name: &str) -> usize {
    cte_refs_dml(body, &[], &mut |r| usize::from(r == name))
}

/// Every plain relation reference a walk reaches, tallied by name.
pub(crate) type CteRefCounts = HashMap<String, usize>;

pub(crate) fn tally_cte_refs(body: &CteBody) -> CteRefCounts {
    let mut counts = CteRefCounts::new();
    cte_refs_dml(body, &[], &mut |r| {
        *counts.entry(r.to_string()).or_insert(0) += 1;
        0
    });
    counts
}

/// The static reference count of every CTE in one `WITH` list, computed in one walk per part
/// (planner.md §3.4). The candidate question — for the CTE at `ci`, how often do the LATER bodies
/// and the primary name it — answered with a per-name walk over the remaining list is O(n²) in the
/// number of CTEs (20 000 CTEs took a minute of unmetered planning). The tally walks each body once,
/// keeping its per-name counts, and answers in cursor order: before CTE `ci` is asked, the counts of
/// bodies `0..=ci` are subtracted from the running totals, which then hold exactly the later bodies'
/// and the primary's references — the same figures as the per-name walks, in O(total size). A
/// data-modifying part's references count apart, because such a part is planned only when it runs,
/// so a CTE it names is never inlined at its reference.
pub(crate) struct CteRefTally {
    /// References by query parts after the cursor + a query primary.
    pub(crate) query: CteRefCounts,
    /// References by data-modifying parts after the cursor + a data-modifying primary.
    pub(crate) dml: CteRefCounts,
    /// Per CTE body, its references by name.
    pub(crate) bodies: Vec<CteRefCounts>,
    /// The first body not yet subtracted.
    pub(crate) next: usize,
}

impl CteRefTally {
    /// Walks every CTE body and the primary (the query that owns the list) once.
    pub(crate) fn new(ctes: &[Cte], primary: CtePrimary<'_>) -> CteRefTally {
        let mut t = CteRefTally {
            query: CteRefCounts::new(),
            dml: CteRefCounts::new(),
            bodies: Vec::with_capacity(ctes.len()),
            next: 0,
        };
        for cte in ctes {
            let counts = tally_cte_refs(&cte.body);
            t.add(&counts, cte.body.is_data_modifying(), false);
            t.bodies.push(counts);
        }
        let (counts, is_dml) = match primary {
            CtePrimary::Body(b) => (tally_cte_refs(b), b.is_data_modifying()),
            CtePrimary::Query(q) => {
                let mut counts = CteRefCounts::new();
                cte_refs_query(q, &[], &mut |r| {
                    *counts.entry(r.to_string()).or_insert(0) += 1;
                    0
                });
                (counts, false)
            }
        };
        t.add(&counts, is_dml, false);
        t
    }

    /// Fold `counts` into (or, `subtract`, out of) the running totals.
    pub(crate) fn add(&mut self, counts: &CteRefCounts, is_dml: bool, subtract: bool) {
        let into = if is_dml {
            &mut self.dml
        } else {
            &mut self.query
        };
        for (name, n) in counts {
            let slot = into.entry(name.clone()).or_insert(0);
            if subtract {
                *slot -= n;
            } else {
                *slot += n;
            }
        }
    }

    /// Whether the CTE at `ci` may be inlined at its reference (planner.md §3.4): not
    /// `MATERIALIZED`, and named exactly once by the later CTE bodies and the primary, by a query
    /// part. The WITH planner asks in list order, so the cursor only ever moves forward.
    pub(crate) fn candidate(&mut self, ctes: &[Cte], ci: usize) -> bool {
        while self.next <= ci {
            let counts = std::mem::take(&mut self.bodies[self.next]);
            self.add(&counts, ctes[self.next].body.is_data_modifying(), true);
            self.bodies[self.next] = counts;
            self.next += 1;
        }
        let name = ctes[ci].name.as_str();
        self.query.get(name).copied().unwrap_or(0) == 1
            && self.dml.get(name).copied().unwrap_or(0) == 0
            && ctes[ci].materialized != Some(true)
    }
}

/// Add the references one NON-planned data-modifying part (a data-modifying CTE body, or a
/// data-modifying primary) contributes to each binding it names (writable-cte.md §3), in one walk
/// of the part (the WITH planner rejects a duplicate name, so a name has at most one binding).
pub(crate) fn add_dml_cte_refs(bindings: &[CteBinding], body: &CteBody) {
    let counts = tally_cte_refs(body);
    if counts.is_empty() {
        return;
    }
    let by_name: HashMap<&str, &CteBinding> =
        bindings.iter().map(|b| (b.name.as_str(), b)).collect();
    for (name, n) in &counts {
        if let Some(b) = by_name.get(name.as_str()) {
            b.refs.set(b.refs.get() + n);
        }
    }
}

/// What a CTE binding evaluates to (spec/design/cte.md, writable-cte.md). A plain CTE holds a
/// planned query body; a **data-modifying** CTE holds the statement to execute (for its effect +
/// `RETURNING` buffer). A data-modifying CTE is always materialized (writable-cte.md §3), so the
/// inline-execution path never touches a `Dml` source.
pub(crate) enum CteSource {
    Query(QueryPlan),
    Dml(DmCte),
}

/// A data-modifying CTE's body (spec/design/writable-cte.md): the `INSERT`/`UPDATE`/`DELETE` to run
/// (cloned from the AST, executed with the statement's CTE context threaded in) and whether it has
/// no `RETURNING` clause — in which case a FROM reference to it is `0A000` (§5).
pub(crate) struct DmCte {
    pub(crate) stmt: DmStmt,
    pub(crate) no_returning: bool,
}

/// A data-modifying statement in a writable-CTE position (a CTE body or the `WITH` primary).
pub(crate) enum DmStmt {
    Insert(Box<Insert>),
    Update(Box<Update>),
    Delete(Box<Delete>),
    Merge(Box<Merge>),
}

/// The recursive half of a `WITH RECURSIVE` CTE (spec/design/recursive-cte.md §4): the planned
/// recursive term (the `UNION`'s right operand, which references the CTE once) and whether the body
/// is `UNION ALL` (keep every row) versus `UNION` (drop rows duplicating any already emitted).
pub(crate) struct RecursiveTerm {
    pub(crate) plan: QueryPlan,
    pub(crate) union_all: bool,
    /// The `SEARCH` / `CYCLE` columns the executor adds to each row (recursive-cte.md §9).
    pub(crate) search_cycle: Option<SearchCyclePlan>,
}

/// How a column reference resolved against the scope CHAIN (spec/design/grammar.md §26).
/// `Local` is a column of one of THIS query's relations (a flat row index into the joined
/// row); `Outer` is a correlated reference to an enclosing query — `level` hops outward
/// (1 = immediate parent) and `index` is the flat column index within that ancestor's row.
#[derive(Clone, Copy)]
pub(crate) enum Resolved {
    Local(usize),
    Outer {
        level: usize,
        index: usize,
    },
    /// Column `col` of the layered relation `rel` (a write through a view, views.md §15.2), found
    /// `level` scopes out (0 = this scope). Resolved to an expression by [`resolve_layer_ref`].
    Layer {
        level: usize,
        rel: usize,
        col: usize,
    },
}

/// A `USING` / `NATURAL` **merged column** (spec/design/grammar.md §15): `name` is the (exact)
/// join column and `index` the flat row index a bare reference to it resolves to — the **surviving
/// side**: the left column for `INNER`/`LEFT`, the right column for `RIGHT`. (`FULL JOIN ... USING`,
/// whose merge is `COALESCE(left, right)` and so is not a single column, is a deferred `0A000`
/// narrowing.) Both underlying copies are recorded in the scope's `hidden` set.
#[derive(Clone)]
pub(crate) struct MergeCol {
    pub(crate) name: String,
    pub(crate) index: usize,
}

/// The relations a query's FROM clause puts in scope, in FROM order, plus the enclosing
/// scope chain (for correlated references — grammar.md §26) and the catalog (so resolving a
/// subquery can look up its own FROM tables).
pub(crate) struct Scope<'a> {
    pub(crate) rels: Vec<ScopeRel<'a>>,
    /// The enclosing query's scope, for correlated-reference resolution (None at top level).
    pub(crate) parent: Option<&'a Scope<'a>>,
    /// The catalog, so a subquery's inner FROM tables can be resolved during planning.
    pub(crate) catalog: &'a Engine,
    /// Whether a subquery is allowed in this scope's expressions: true inside a SELECT (and
    /// its nested subqueries), false for UPDATE/DELETE (a subquery there is 0A000 this slice).
    pub(crate) allow_subquery: bool,
    /// The statement's CTE bindings visible here (spec/design/cte.md §2). Inherited DIRECTLY down
    /// into nested scopes (a subquery sees the same `ctes`), NOT via the `parent` chain — so CTE
    /// lookup never counts as a correlation level. Empty for every non-`WITH` statement.
    pub(crate) ctes: &'a [&'a CteBinding],
    /// `USING` / `NATURAL` merged columns (spec/design/grammar.md §15) — a bare reference to a merge
    /// name resolves to its `index` (checked before the per-relation search, so it is never the
    /// underlying copies' `42702` ambiguity). Empty for every scope except a SELECT whose FROM has a
    /// `USING`/`NATURAL` join.
    pub(crate) merges: Vec<MergeCol>,
    /// Flat indices SUPERSEDED by a merge — the underlying left+right copies, omitted from `*`
    /// expansion (still reachable qualified) — plus the copies an unaliased parenthesized join
    /// carries after its star columns. Empty unless the FROM has a `USING`/`NATURAL` join.
    pub(crate) hidden: Vec<usize>,
    /// The relation labels an UNALIASED parenthesized join keeps visible (spec/design/grammar.md
    /// §15): `(a JOIN b ON …)` is one FROM relation, yet `a.x` and `b.*` still name its inner
    /// relations. Reached only by qualified references; never physical.
    pub(crate) sub_rels: Vec<SubRel<'a>>,
    /// The flat indices bare `*` expands to, in order (grammar.md §15): PostgreSQL's per-join
    /// layout — each `USING`/`NATURAL` join's merged columns, then its left input's remaining
    /// columns, then its right input's — concatenated across comma items. `None` for a scope the
    /// FROM-join planner did not build (then `*` is every non-hidden column in FROM order).
    pub(crate) star: Option<Vec<usize>>,
    /// A PERSISTED index expression's scope (an expression key or a partial-index predicate —
    /// spec/design/collation.md §1 "Function results"): collation derivation keeps the storage rule
    /// (a column, `COLLATE`, `||`; a function result carries none), so stored key bytes and
    /// partial-index membership never change meaning under the query-side function propagation.
    pub(crate) storage_collation: bool,
    /// Each scalar subquery resolved against this scope, keyed by its AST node, with its output
    /// column's collation derivation (spec/design/collation.md §1 "Scalar subqueries") and typmod
    /// (spec/design/create-table-as.md §2). `resolve` records them when it plans the body, so
    /// `derive_collation` / `derive_typmod` — which work on the AST — can read them without planning
    /// the body again. Keyed by address: a lookup always follows the resolve of the same node, which
    /// overwrites any stale entry.
    pub(crate) subquery_derivs:
        std::cell::RefCell<Vec<(*const crate::ast::QueryExpr, Deriv, Typmod)>>,
    /// Set on a LATERAL FENCE only (spec/design/grammar.md §15): the empty scope a parenthesized
    /// join's body is planned against when the join is not the first item of its FROM. The fence
    /// hides the relations to the join's left from the body's ON clauses and non-LATERAL items
    /// (PostgreSQL's lateral-only namespace entries), while this view — the prefix of those
    /// relations, at the same correlation level — is what the body's LATERAL items and table
    /// functions resolve against. Only a parenthesized-join body reads it; any other SELECT planned
    /// against a fence strips it.
    pub(crate) lateral_view: Option<&'a Scope<'a>>,
    /// Set when a column reference resolves to one of THIS scope's relations. Read on a
    /// parenthesized join's lateral prefix to learn whether the join's body correlates to the
    /// relations on its left (grammar.md §15).
    pub(crate) lateral_hit: std::cell::Cell<bool>,
    /// `Some(keys)` marks this scope as GROUPED for the subqueries planned under it: the flat input
    /// indices of its grouping keys in group-row slot order (the planner's master list,
    /// functionally dependent columns included — aggregates.md §4). Set only while a collecting
    /// `AggCtx` plans a subquery (`plan_subquery`): the projection, HAVING, ORDER BY and window keys
    /// of a grouped query evaluate over the GROUP row (keys, then aggregates), so a correlated
    /// reference into this scope from such a subquery must name a grouping column — else 42803, as
    /// in PostgreSQL — and reads that key's slot (`resolve_column_ref`). A subquery in WHERE or an
    /// aggregate argument sees the flat input row (`None` there).
    pub(crate) grouped_keys: std::cell::RefCell<Option<Vec<usize>>>,
    /// Set on a LATERAL prefix whose relations are the left side of a `RIGHT` or `FULL` join
    /// (PostgreSQL's `p_lateral_ok = false`, spec/design/grammar.md §44): a reference that resolves
    /// to one of them is `42P10`. A reference that passes them to an outer level is unaffected.
    pub(crate) lateral_refused: bool,
    /// Set on a LATERAL prefix inside a parenthesized join's body (spec/design/grammar.md §15): its
    /// parent — the fence's view — holds relations PostgreSQL places in the same FROM namespace, so a
    /// bare column matching in both is ambiguous (`42702`), not shadowed.
    pub(crate) shares_parent_level: bool,
    /// What a name-resolution hint needs beyond the visible relations (error-fields.md §11.8).
    pub(crate) hints: ScopeHints,
    /// The flat slot `merge_action()` reads: set only on a MERGE's RETURNING scope
    /// (spec/design/merge.md §7). Elsewhere the function is 42601.
    pub(crate) merge_action: Option<usize>,
}

/// A relation of a query level's FROM clause that a reference there cannot see. PostgreSQL's
/// range table holds such relations, and its `42703` / `42P01` hints search them
/// (spec/design/error-fields.md §11.8).
#[derive(Clone)]
pub(crate) struct UnseenRel {
    pub(crate) label: String,
    pub(crate) columns: Vec<String>,
    /// The base table or view the relation names (the §11.2 search), else `None`.
    pub(crate) catalog: Option<String>,
    /// Marking the reference's subquery `LATERAL` would make the relation visible.
    pub(crate) lateral_ok: bool,
    /// The relation is an inner relation of a completed unaliased join, so its columns are
    /// reachable only through its qualifier.
    pub(crate) qualified_only: bool,
}

/// The relations to the left of a non-LATERAL derived table or parenthesized join, recorded while
/// its body is planned (spec/design/error-fields.md §11.8). `depth` is the scope-chain depth of the
/// scope the body is planned against, so the hint walk visits the relations right after the body's
/// own outermost level. `paren_join` marks a parenthesized join's body: its ON clauses cannot reach
/// these relations even with `LATERAL`.
pub(crate) struct UnseenFrame {
    pub(crate) depth: usize,
    pub(crate) paren_join: bool,
    pub(crate) rels: Vec<UnseenRel>,
}

/// A scope's hint-only facts (spec/design/error-fields.md §11.8). They never affect resolution.
#[derive(Default)]
pub(crate) struct ScopeHints {
    /// This level's FROM relations the scope cannot see, before its own: a JOIN's ON clause cannot
    /// see the comma items to its left.
    pub(crate) unseen: Vec<UnseenRel>,
    /// The scope resolves a JOIN's ON clause.
    pub(crate) on_clause: bool,
    /// The scope resolves a table function's arguments (PostgreSQL's `p_lateral_active`): a hint
    /// never suggests `LATERAL` from here.
    pub(crate) lateral_active: bool,
}

/// One relation the hint walk visits (spec/design/error-fields.md §11.8), in walk order.
pub(crate) struct HintEntry {
    pub(crate) label: String,
    pub(crate) columns: Vec<String>,
    pub(crate) catalog: Option<String>,
    pub(crate) visible: bool,
    pub(crate) lateral_ok: bool,
    pub(crate) qualified_only: bool,
}

/// The number of levels in a scope chain (0 for none).
pub(crate) fn scope_depth(scope: Option<&Scope>) -> usize {
    let mut depth = 0;
    let mut level = scope;
    while let Some(s) = level {
        depth += 1;
        level = s.parent;
    }
    depth
}

/// One inner relation label of an unaliased parenthesized join (grammar.md §15): its table's
/// columns live at the flat slots `slots` of the join's single FROM relation (slot k holds column
/// k). A scope's borrowed view of a [`GroupSubRel`].
#[derive(Clone)]
pub(crate) struct SubRel<'a> {
    pub(crate) label: String,
    pub(crate) table: &'a Table,
    pub(crate) slots: Vec<usize>,
}

/// The owned form of a [`SubRel`], held by a [`JoinGroupLayout`] and by the FROM build that
/// registers a join group's inner labels (the scope's `SubRel`s borrow its table). `rel` is the
/// owning FROM ordinal, so a partial ON scope sees only the labels of the relations it spans.
pub(crate) struct GroupSubRel {
    pub(crate) label: String,
    pub(crate) table: Box<Table>,
    pub(crate) slots: Vec<usize>,
    pub(crate) rel: usize,
}

/// The output shape of an unaliased parenthesized join's body (spec/design/grammar.md §15):
/// `columns` are the body outputs (the star columns, then the hidden USING/NATURAL copies), each
/// keeping its source column's name, type, and collation; `hidden` is the output positions of those
/// copies; `sub_rels` give each inner label's columns as output positions (`rel` unused).
pub(crate) struct JoinGroupLayout {
    pub(crate) columns: Vec<Column>,
    pub(crate) hidden: Vec<usize>,
    pub(crate) sub_rels: Vec<GroupSubRel>,
}

/// The borrowed sub-relation labels owned by FROM ordinals `lo..=hi`.
pub(crate) fn sub_rels_within(subs: &[GroupSubRel], lo: usize, hi: usize) -> Vec<SubRel<'_>> {
    subs.iter()
        .filter(|sr| sr.rel >= lo && sr.rel <= hi)
        .map(|sr| SubRel {
            label: sr.label.clone(),
            table: &sr.table,
            slots: sr.slots.clone(),
        })
        .collect()
}

impl<'a> Scope<'a> {
    /// A one-relation scope with no parent (the single-table UPDATE / DELETE case). Subqueries
    /// ARE allowed: a correlated reference resolves to the target row via the per-row outer
    /// environment (the subquery's parent is this scope), an uncorrelated one folds once
    /// (spec/design/grammar.md §26). SELECT builds its own scope in `plan_select`.
    pub(crate) fn single(catalog: &'a Engine, table: &'a Table) -> Scope<'a> {
        Scope {
            rels: vec![ScopeRel {
                label: table.name.to_string(),
                table,
                offset: 0,
                qualifier_only: false,
                view: None,
                base: false,
                cte: None,
                db: None,
                layer: None,
            }],
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
            sub_rels: Vec::new(),
            star: None,
            storage_collation: false,
            subquery_derivs: Default::default(),
            lateral_view: None,
            lateral_hit: Default::default(),
            grouped_keys: Default::default(),
            lateral_refused: false,
            shares_parent_level: false,
            merge_action: None,
            hints: Default::default(),
        }
    }

    /// The target scope of a write (UPDATE / DELETE): [`Scope::single`], or, for a write through a
    /// view, the base table relabeled with the view's name and exposing the view's columns
    /// (spec/design/views.md §15.2).
    pub(crate) fn write_target(
        catalog: &'a Engine,
        table: &'a Table,
        vw: Option<&'a ViewWrite>,
    ) -> Scope<'a> {
        let mut scope = Scope::single(catalog, table);
        if let Some(vw) = vw {
            scope.rels[0].label = vw.view.clone();
            scope.rels[0].layer = Some(&vw.top);
        }
        scope
    }

    /// A scope with no relations under `parent`: one level between a reference to a view's
    /// read-only column and the view body's scope the column's select item resolves in (views.md
    /// §15.2), so the item's names bind only there.
    pub(crate) fn blank_child(parent: &'a Scope<'a>) -> Scope<'a> {
        let mut scope = Scope::empty(parent.catalog);
        scope.parent = Some(parent);
        scope.allow_subquery = true;
        scope
    }

    /// The isolated scope a view body's select items and WHERE resolve in during a write through
    /// the view (views.md §4.1, §15.2): the body's single FROM item, labeled `label`, exposing
    /// `layer`'s columns over the base row at flat `offset`. No parent and no CTEs.
    pub(crate) fn layer_body(
        catalog: &'a Engine,
        table: &'a Table,
        offset: usize,
        label: &str,
        layer: &'a WriteLayer,
    ) -> Scope<'a> {
        let mut scope = Scope::empty(catalog);
        scope.allow_subquery = true;
        scope.rels.push(ScopeRel {
            label: label.to_string(),
            table,
            offset,
            qualifier_only: false,
            view: None,
            base: false,
            cte: None,
            db: None,
            layer: Some(layer),
        });
        scope
    }

    /// The scope a PERSISTED index expression (an expression key or a partial-index predicate)
    /// resolves against: [`Scope::single`] with the storage collation rule (`storage_collation`).
    pub(crate) fn index(catalog: &'a Engine, table: &'a Table) -> Scope<'a> {
        let mut scope = Scope::single(catalog, table);
        scope.storage_collation = true;
        scope
    }

    /// A column-less scope — the environment a `DEFAULT` expression resolves against
    /// (constraints.md §2): a default may not reference a column (rejected as 0A000 by the
    /// structural pre-walk before resolution) and may not contain a subquery, so there are no
    /// relations and subqueries are disallowed.
    pub(crate) fn empty(catalog: &'a Engine) -> Scope<'a> {
        Scope {
            rels: Vec::new(),
            parent: None,
            catalog,
            allow_subquery: false,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
            sub_rels: Vec::new(),
            star: None,
            storage_collation: false,
            subquery_derivs: Default::default(),
            lateral_view: None,
            lateral_hit: Default::default(),
            grouped_keys: Default::default(),
            lateral_refused: false,
            shares_parent_level: false,
            merge_action: None,
            hints: Default::default(),
        }
    }

    /// The scope a RETURNING list resolves against (grammar.md §32): the target table at
    /// offset 0 (bare and table-qualified references read the BASE row), plus the `old`/`new`
    /// row-version pseudo-relations as QUALIFIER-ONLY rels over the concatenated projection
    /// row `[base | other]`. `base_is_old` says which version the base row is: false for
    /// INSERT/UPDATE (base = the new row, `old` reads the other half), true for DELETE
    /// (base = the old row, `new` reads the other half) — the absent version is the all-NULL
    /// row the caller appends. Explicit `WITH (OLD AS o, NEW AS n)` aliases are installed
    /// first and hide that version's standard name; an unaliased default is suppressed when
    /// its label is already occupied (including by the target table). Explicit aliases may not
    /// collide with the target or each other (`42712`). An INSERT's `AS alias` (upsert.md §5)
    /// replaces the target's label, so the table (or view) name is no longer a visible qualifier —
    /// referencing it is `42P01` with the table-alias hint (error-fields.md §11.2).
    pub(crate) fn returning(
        catalog: &'a Engine,
        table: &'a Table,
        base_is_old: bool,
        returning: &ReturningClause,
        vw: Option<&'a ViewWrite>,
        alias: Option<&str>,
    ) -> Result<Scope<'a>> {
        let n = table.columns.len();
        let layer = vw.map(|vw| &vw.top);
        let (old_offset, new_offset) = if base_is_old { (0, n) } else { (n, 0) };
        let mut rels = vec![Scope::target_rel(table, vw, alias)];
        for (alias, offset) in [
            (returning.old_alias.as_deref(), old_offset),
            (returning.new_alias.as_deref(), new_offset),
        ] {
            if let Some(alias) = alias {
                let alias = alias.to_string();
                if rels.iter().any(|r| r.label == alias) {
                    return Err(EngineError::new(
                        SqlState::DuplicateAlias,
                        format!("table name {alias} specified more than once"),
                    ));
                }
                rels.push(ScopeRel {
                    label: alias,
                    table,
                    offset,
                    qualifier_only: true,
                    view: None,
                    base: false,
                    cte: None,
                    db: None,
                    layer,
                });
            }
        }
        for (pseudo, offset, aliased) in [
            ("old", old_offset, returning.old_alias.is_some()),
            ("new", new_offset, returning.new_alias.is_some()),
        ] {
            if !aliased && !rels.iter().any(|r| r.label == pseudo) {
                rels.push(ScopeRel {
                    label: pseudo.to_string(),
                    table,
                    offset,
                    qualifier_only: true,
                    view: None,
                    base: false,
                    cte: None,
                    db: None,
                    layer,
                });
            }
        }
        Ok(Scope {
            rels,
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
            sub_rels: Vec::new(),
            star: None,
            storage_collation: false,
            subquery_derivs: Default::default(),
            lateral_view: None,
            lateral_hit: Default::default(),
            grouped_keys: Default::default(),
            lateral_refused: false,
            shares_parent_level: false,
            merge_action: None,
            hints: Default::default(),
        })
    }

    /// The target relation of an INSERT's RETURNING / DO UPDATE scope (or an UPDATE/DELETE's
    /// RETURNING) at offset 0: labeled by the INSERT's `AS alias` when given (upsert.md §5), else by
    /// the view's name for a write through a view (views.md §15.2), else by the table's name. The
    /// catalog name is recorded (`base` / `view`) so a reference by the hidden table or view name
    /// gets the table-alias hint (error-fields.md §11.2).
    pub(crate) fn target_rel(
        table: &'a Table,
        vw: Option<&'a ViewWrite>,
        alias: Option<&str>,
    ) -> ScopeRel<'a> {
        let catalog_label = match vw {
            Some(vw) => vw.view.clone(),
            None => table.name.to_string(),
        };
        ScopeRel {
            label: alias.map_or(catalog_label, str::to_string),
            table,
            offset: 0,
            qualifier_only: false,
            view: vw.map(|vw| vw.view.clone()),
            base: vw.is_none(),
            cte: None,
            db: None,
            layer: vw.map(|vw| &vw.top),
        }
    }

    /// The scope a DO UPDATE's `SET`/`WHERE` resolve against (spec/design/upsert.md §5): the
    /// target table at offset 0 — labeled by the INSERT's `AS alias` when given — whose bare and
    /// label-qualified references read the EXISTING conflicting row, plus `excluded` as a
    /// QUALIFIER-ONLY relation at offset `n` over the combined row `[existing | proposed]`
    /// (`excluded.col` reads the proposed row). A target labeled `excluded` (a table so named, or
    /// `AS excluded`) keeps BOTH relations under the one label, so `excluded.col` is `42P09`
    /// (PostgreSQL's rule — `resolve_qualified`). For an INSERT through a view (`vw`, views.md
    /// §15.7) both relations expose the view's columns over their halves of the combined row.
    pub(crate) fn on_conflict_excluded(
        catalog: &'a Engine,
        table: &'a Table,
        vw: Option<&'a ViewWrite>,
        alias: Option<&str>,
    ) -> Scope<'a> {
        let n = table.columns.len();
        let rels = vec![
            Scope::target_rel(table, vw, alias),
            ScopeRel {
                label: "excluded".to_string(),
                table,
                offset: n,
                qualifier_only: true,
                view: None,
                base: false,
                cte: None,
                db: None,
                layer: vw.map(|vw| &vw.top),
            },
        ];
        Scope {
            rels,
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
            sub_rels: Vec::new(),
            star: None,
            storage_collation: false,
            subquery_derivs: Default::default(),
            lateral_view: None,
            lateral_hit: Default::default(),
            grouped_keys: Default::default(),
            lateral_refused: false,
            shares_parent_level: false,
            merge_action: None,
            hints: Default::default(),
        }
    }

    /// Resolve a bare name in expression position: a column ([`resolve_bare_column`]) when any
    /// scope in the chain has one, else a WHOLE-ROW reference to a scalar function relation of that
    /// label ([`resolve_scalar_func_row`]) — PostgreSQL's `transformColumnRef` order, which tries
    /// every level for a column before trying the name as a relation. 42703 when neither matches.
    ///
    /// [`resolve_bare_column`]: Scope::resolve_bare_column
    /// [`resolve_scalar_func_row`]: Scope::resolve_scalar_func_row
    pub(crate) fn resolve_bare(&self, name: &str) -> Result<Resolved> {
        match self.resolve_bare_column(name) {
            Err(e) if e.state == SqlState::UndefinedColumn => {
                self.resolve_scalar_func_row(name).unwrap_or(Err(e))
            }
            r => r,
        }
    }

    /// A reference resolved to this scope's relation `label`: record the hit (`lateral_hit`), or
    /// refuse it when this scope is the left side of a `RIGHT`/`FULL` join — PostgreSQL's
    /// `check_lateral_ref_ok`, raised at the first matching relation (grammar.md §44).
    pub(crate) fn hit(&self, label: &str) -> Result<()> {
        self.refuse(label)?;
        self.lateral_hit.set(true);
        Ok(())
    }

    /// `42P10` when this scope is the left side of a `RIGHT`/`FULL` join (`hit`).
    pub(crate) fn refuse(&self, label: &str) -> Result<()> {
        if self.lateral_refused {
            return Err(EngineError::new(
                SqlState::InvalidColumnReference,
                format!("invalid reference to FROM-clause entry for table {label}"),
            )
            .with_detail(Some(
                "The combining JOIN type must be INNER or LEFT for a LATERAL reference.".into(),
            )));
        }
        Ok(())
    }

    /// Whether the fence views PostgreSQL places in this scope's FROM namespace (`shares_parent_level`,
    /// grammar.md §15) have a visible column `name`, raising that namespace's errors in its order —
    /// outermost first: a second match is `42702`, a refused first match `42P10`.
    pub(crate) fn parent_level_has_column(&self, name: &str) -> Result<bool> {
        match self.parent {
            Some(p) if self.shares_parent_level => p.level_has_column(name),
            _ => Ok(false),
        }
    }

    /// [`parent_level_has_column`](Scope::parent_level_has_column), then this scope's own columns.
    pub(crate) fn level_has_column(&self, name: &str) -> Result<bool> {
        let mut seen = self.parent_level_has_column(name)?;
        let mut matched = |label: &str| -> Result<()> {
            if seen {
                return Err(ambiguous_column(name));
            }
            self.refuse(label)?;
            seen = true;
            Ok(())
        };
        if self.merges.iter().any(|m| m.name == name) {
            matched("unnamed_join")?;
        }
        for r in &self.rels {
            if r.qualifier_only {
                continue;
            }
            if let Some(layer) = r.layer {
                for c in &layer.columns {
                    if c.name == name {
                        matched(&r.label)?;
                    }
                }
                continue;
            }
            for (local, c) in r.table.columns.iter().enumerate() {
                let idx = r.offset + local;
                if c.name == name && !self.hidden.contains(&idx) {
                    matched(&self.refused_label(r, idx))?;
                }
            }
        }
        Ok(seen)
    }

    /// The label PostgreSQL names a relation by when a bare column `idx` of this scope is refused
    /// (`hit`): its label, else `unnamed_join` for an unaliased parenthesized join's column, else
    /// `unnamed_subquery` (an unaliased derived table).
    pub(crate) fn refused_label(&self, rel: &ScopeRel, idx: usize) -> String {
        if !rel.label.is_empty() {
            rel.label.clone()
        } else if self.sub_rels.iter().any(|sr| sr.slots.contains(&idx)) {
            "unnamed_join".to_string()
        } else {
            "unnamed_subquery".to_string()
        }
    }

    /// Resolve a bare name that labels a scalar set-returning function's relation as a whole-row
    /// reference to it (spec/design/functions.md §10): a scalar function's row IS its one column,
    /// so `SELECT e FROM jsonb_array_elements(…) AS e` reads the element (PostgreSQL's
    /// `makeWholeRowVar` for a scalar function). Searched THIS scope, then outward (nearest wins).
    /// No other relation kind has a whole-row value in jed (no implicit per-table row types).
    pub(crate) fn resolve_scalar_func_row(&self, name: &str) -> Option<Result<Resolved>> {
        for r in &self.rels {
            if r.qualifier_only || r.layer.is_some() || r.label != name || !r.table.scalar_func {
                continue;
            }
            return Some(self.hit(name).map(|()| Resolved::Local(r.offset)));
        }
        // An unaliased parenthesized join keeps its inner function relation's label visible.
        if let Some(sr) = self
            .sub_rels
            .iter()
            .find(|sr| sr.label == name && sr.table.scalar_func)
        {
            return Some(self.hit(name).map(|()| Resolved::Local(sr.slots[0])));
        }
        self.parent
            .and_then(|p| p.resolve_scalar_func_row(name))
            .map(|r| r.map(outer_of))
    }

    /// Resolve a bare column name against THIS scope, then OUTWARD through the parent chain.
    /// Within one scope: two+ relations have it → 42702 ambiguous; exactly one → `Local`; none
    /// → fall through to the parent. A name found only in an ancestor is an `Outer` reference
    /// (nearest scope wins — an inner match shadows an outer one, matching PostgreSQL). 42703
    /// only if no scope in the chain has it. A qualifier-only rel (the RETURNING `old`/`new`
    /// pseudo-relations) is invisible here — no new ambiguity (grammar.md §32). A column only,
    /// never a whole-row reference: a `USING` column resolves through this directly.
    pub(crate) fn resolve_bare_column(&self, name: &str) -> Result<Resolved> {
        // A USING/NATURAL MERGE column resolves to its surviving side (grammar.md §15), seeded here
        // so the bare name binds the merged column rather than its two (hidden) underlying copies —
        // which is why such a join column is unambiguous. A *non-hidden* column elsewhere with the
        // same name still makes the reference ambiguous (a third relation sharing the name).
        // A match in the fence views of the same FROM namespace comes first (§15).
        let level_seen = self.parent_level_has_column(name)?;
        let mut found: Option<usize> = self.merges.iter().find(|m| m.name == name).map(|m| m.index);
        if found.is_some() {
            if level_seen {
                return Err(ambiguous_column(name));
            }
            self.hit("unnamed_join")?;
        }
        let mut layered: Option<Resolved> = None;
        for (ri, r) in self.rels.iter().enumerate() {
            if r.qualifier_only {
                continue;
            }
            // A write through a view exposes the view's columns (views.md §15.2).
            if let Some(layer) = r.layer {
                for (col, c) in layer.columns.iter().enumerate() {
                    if c.name == name {
                        if level_seen || found.is_some() || layered.is_some() {
                            return Err(ambiguous_column(name));
                        }
                        self.hit(&r.label)?;
                        layered = Some(Resolved::Layer {
                            level: 0,
                            rel: ri,
                            col,
                        });
                    }
                }
                continue;
            }
            // Count EVERY matching column, not just the first per relation: a synthetic relation (a
            // CTE or derived table) may carry two columns of the same name, and a bare reference to
            // that name is ambiguous (42702) exactly as a match across two relations is (cte.md §2,
            // grammar.md §42). Base tables have unique column names, so this only ever fires for a
            // duplicate-output-name synthetic relation.
            for (local, c) in r.table.columns.iter().enumerate() {
                let idx = r.offset + local;
                // A merge's underlying copies are superseded by the merge above — skip them.
                if self.hidden.contains(&idx) {
                    continue;
                }
                if c.name == name {
                    if level_seen || found.is_some() || layered.is_some() {
                        return Err(ambiguous_column(name));
                    }
                    self.hit(&self.refused_label(r, idx))?;
                    found = Some(idx);
                }
            }
        }
        if let Some(r) = layered {
            return Ok(r);
        }
        if let Some(idx) = found {
            self.trace_flat(idx);
            return Ok(Resolved::Local(idx));
        }
        match self.parent {
            Some(p) => Ok(outer_of(p.resolve_bare_column(name)?)),
            None => Err(undefined_column(name)),
        }
    }

    /// The label a frozen `*` column renders with (spec/design/views.md §5): `None` (a bare
    /// `"column"`) for a `USING`/`NATURAL` merged column and for a relation with no label (an
    /// unaliased derived table); an unaliased parenthesized join's column takes its inner relation's
    /// label; any other column its relation's label.
    pub(crate) fn star_label(&self, idx: usize) -> Option<String> {
        if self.merges.iter().any(|m| m.index == idx) {
            return None;
        }
        for r in &self.rels {
            let n = r.table.columns.len();
            if idx >= r.offset && idx < r.offset + n {
                if !r.label.is_empty() {
                    return Some(r.label.clone());
                }
                return self
                    .sub_rels
                    .iter()
                    .find(|sr| sr.slots.contains(&idx))
                    .map(|sr| sr.label.clone());
            }
        }
        None
    }

    /// Record a resolved column of this scope as a view dependency when a view body is being
    /// traced and the column belongs to a catalog base table (spec/design/views.md §6). A no-op on
    /// every ordinary statement.
    pub(crate) fn trace_flat(&self, idx: usize) {
        if !self.catalog.view_tracing() {
            return;
        }
        for r in &self.rels {
            let n = r.table.columns.len();
            if idx >= r.offset && idx < r.offset + n {
                self.catalog.trace_rel_column(r, idx - r.offset);
                return;
            }
        }
    }

    /// Resolve a qualified `rel.col` against THIS scope, then outward. A qualifier that names a
    /// relation in this scope binds here — a missing column is then 42703 (no fall-through, so
    /// an inner relation shadows a same-named outer one). Only an unknown qualifier walks
    /// outward (42P01 if no ancestor has it).
    pub(crate) fn resolve_qualified(&self, qualifier: &str, name: &str) -> Result<Resolved> {
        let q = qualifier.to_string();
        // Two relations under one label at this level — only the DO UPDATE scope of a target
        // labeled `excluded` builds one (every FROM rejects a duplicate label up front, 42712):
        // PostgreSQL's 42P09 (upsert.md §5).
        // A relation label that is also an inner label of a joined source (a MERGE target aliased
        // like one of its source join's relations — merge.md §2) is likewise ambiguous: PostgreSQL's
        // 42P09, never a silent bind to the target.
        let labeled = self.rels.iter().filter(|r| r.label == q).count()
            + usize::from(self.sub_rels.iter().any(|sr| sr.label == q));
        if labeled > 1 {
            return Err(EngineError::new(
                SqlState::AmbiguousAlias,
                format!("table reference {qualifier} is ambiguous"),
            ));
        }
        if let Some(ri) = self
            .rels
            .iter()
            .position(|r| r.label == q && r.layer.is_some())
        {
            let layer = self.rels[ri].layer.expect("a layered relation");
            self.hit(qualifier)?;
            let mut hits = layer
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name == name);
            return match (hits.next(), hits.next()) {
                (None, _) => Err(undefined_column(name)),
                (Some(_), Some(_)) => Err(ambiguous_column(name)),
                (Some((col, _)), None) => Ok(Resolved::Layer {
                    level: 0,
                    rel: ri,
                    col,
                }),
            };
        }
        if let Some(rel) = self.rels.iter().find(|r| r.label == q) {
            // PostgreSQL refuses the relation itself, before looking up the column.
            self.hit(qualifier)?;
            // A synthetic relation (a derived table, an aliased parenthesized join) may carry two
            // columns of one name; a qualified reference to it is ambiguous (42702) exactly as a bare
            // one is (PG). A USING/NATURAL copy hidden by a merge yields to the visible column.
            let mut found: Option<usize> = None;
            let mut visible: Option<usize> = None;
            let mut ambiguous = false;
            for (local, c) in rel.table.columns.iter().enumerate() {
                if c.name != name {
                    continue;
                }
                let idx = rel.offset + local;
                if found.is_none() {
                    found = Some(idx);
                }
                if !self.hidden.contains(&idx) {
                    ambiguous = ambiguous || visible.is_some();
                    visible = Some(idx);
                }
            }
            return match (found, visible) {
                (None, _) => Err(undefined_column(name)),
                _ if ambiguous => Err(ambiguous_column(name)),
                (_, Some(idx)) | (Some(idx), None) => {
                    self.trace_flat(idx);
                    Ok(Resolved::Local(idx))
                }
            };
        }
        if let Some(sr) = self.sub_rels.iter().find(|sr| sr.label == q) {
            self.hit(qualifier)?;
            let local = sr
                .table
                .column_index(name)
                .ok_or_else(|| undefined_column(name))?;
            return Ok(Resolved::Local(sr.slots[local]));
        }
        match self.parent {
            Some(p) => Ok(outer_of(p.resolve_qualified(qualifier, name)?)),
            None => Err(missing_from_entry(qualifier)),
        }
    }

    /// [`resolve_bare`](Scope::resolve_bare) for a column reference written at `pos`: a
    /// name-resolution error carries the reference's position and its hint
    /// (spec/design/error-fields.md §10.2, §11).
    pub(crate) fn resolve_bare_at(&self, name: &str, pos: SrcPos) -> Result<Resolved> {
        self.resolve_bare(name)
            .map_err(|e| self.locate_column_error(e, None, name, pos))
    }

    /// [`resolve_qualified`](Scope::resolve_qualified) for a reference written at `pos` (§10.2, §11).
    pub(crate) fn resolve_qualified_at(
        &self,
        qualifier: &str,
        name: &str,
        pos: SrcPos,
    ) -> Result<Resolved> {
        self.resolve_qualified(qualifier, name)
            .map_err(|e| self.locate_column_error(e, Some(qualifier), name, pos))
    }

    /// Stamp a column reference's `42703` / `42702` / `42P01` / `42P10` with its position, and add the
    /// misspelled-column (§11.1), table-alias (§11.2), or LATERAL (§11.8) hint. An error already
    /// positioned came from an inner site, which wins.
    pub(crate) fn locate_column_error(
        &self,
        e: EngineError,
        qualifier: Option<&str>,
        name: &str,
        pos: SrcPos,
    ) -> EngineError {
        if e.position.is_some()
            || !matches!(
                e.state,
                SqlState::UndefinedColumn
                    | SqlState::AmbiguousColumn
                    | SqlState::UndefinedTable
                    | SqlState::AmbiguousAlias
                    | SqlState::InvalidColumnReference
            )
        {
            return e;
        }
        let (hint, detail) = match (e.state, qualifier) {
            (SqlState::UndefinedColumn, _) => self.column_hint(qualifier, name),
            (SqlState::UndefinedTable, Some(q)) => self.table_hint(q),
            _ => (None, None),
        };
        let e = e.with_position(pos.0).with_hint(hint);
        if e.detail.is_none() {
            e.with_detail(detail)
        } else {
            e
        }
    }

    /// The labeled relations this level can see, in the §11.1 walk order: the FROM relations in
    /// order, then the inner relations of an unaliased parenthesized join. Each yields its label,
    /// column names, and the base table or view it names (§11.2).
    pub(crate) fn hint_relations(&self) -> Vec<(&str, Vec<&str>, Option<&str>)> {
        let mut out = Vec::new();
        for r in &self.rels {
            if r.label.is_empty() {
                continue;
            }
            let cols = match r.layer {
                Some(layer) => layer.columns.iter().map(|c| c.name.as_str()).collect(),
                None => r.table.columns.iter().map(|c| c.name.as_str()).collect(),
            };
            let catalog = match &r.view {
                Some(v) => Some(v.as_str()),
                None if r.base => Some(r.table.name.as_str()),
                None => None,
            };
            out.push((r.label.as_str(), cols, catalog));
        }
        for sr in &self.sub_rels {
            if !sr.label.is_empty() {
                let cols = sr.table.columns.iter().map(|c| c.name.as_str()).collect();
                out.push((sr.label.as_str(), cols, None));
            }
        }
        out
    }

    /// The scope-chain depth of this scope (1 for a scope with no parent).
    pub(crate) fn depth(&self) -> usize {
        scope_depth(Some(self))
    }

    /// Every relation a hint searches, in PostgreSQL's range-table walk order (spec/design/
    /// error-fields.md §11.1, §11.8): at each level outward, the relations this level cannot see to
    /// the left of an ON clause, the visible relations, then the relations hidden from the derived
    /// bodies planned against the level's parent (innermost first).
    pub(crate) fn hint_walk(&self) -> Vec<HintEntry> {
        let frames = self.catalog.unseen_frames.borrow();
        let lateral_active = self.hints.lateral_active;
        let unseen = |u: &UnseenRel, blocked: bool| HintEntry {
            label: u.label.clone(),
            columns: u.columns.clone(),
            catalog: u.catalog.clone(),
            visible: false,
            lateral_ok: u.lateral_ok && !blocked && !lateral_active,
            qualified_only: u.qualified_only && !blocked,
        };
        let mut out = Vec::new();
        let mut depth = self.depth();
        let mut level = Some(self);
        while let Some(s) = level {
            for u in &s.hints.unseen {
                out.push(unseen(u, true));
            }
            for (label, cols, catalog) in s.hint_relations() {
                out.push(HintEntry {
                    label: label.to_string(),
                    columns: cols.into_iter().map(str::to_string).collect(),
                    catalog: catalog.map(str::to_string),
                    visible: true,
                    lateral_ok: false,
                    qualified_only: false,
                });
            }
            depth -= 1;
            for f in frames.iter().rev().filter(|f| f.depth == depth) {
                // A parenthesized join's ON clause cannot see the relations to the join's left.
                let blocked = s.hints.on_clause && f.paren_join;
                for u in &f.rels {
                    out.push(unseen(u, blocked));
                }
            }
            level = s.parent;
        }
        out
    }

    /// PostgreSQL's `errorMissingColumn` hint and detail (spec/design/error-fields.md §11.1, §11.8):
    /// a relation that has the column but cannot be seen from here yields the "cannot be referenced"
    /// detail and the LATERAL or qualified-name hint; otherwise the misspelled-column suggestion.
    pub(crate) fn column_hint(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> (Option<String>, Option<String>) {
        let entries = self.hint_walk();
        let mut m = error_hint::FuzzyColumnMatch::new(qualifier, name);
        let mut exact: Vec<&HintEntry> = Vec::new();
        for e in &entries {
            let penalty = m.penalty(&e.label);
            for col in &e.columns {
                m.offer(&e.label, penalty, col);
            }
            if penalty == 0 && e.columns.iter().any(|c| c == name) {
                exact.push(e);
            }
        }
        match exact.as_slice() {
            [] => (m.hint(), None),
            [e] => {
                let hint = if e.lateral_ok {
                    Some(error_hint::LATERAL_COLUMN.to_string())
                } else if qualifier.is_none() && e.qualified_only {
                    Some(error_hint::QUALIFY_COLUMN.to_string())
                } else {
                    None
                };
                let detail = error_hint::unreachable_column_detail(name, &e.label);
                (hint, Some(detail))
            }
            _ => (
                qualifier
                    .is_none()
                    .then(|| error_hint::TRY_QUALIFIED.to_string()),
                Some(error_hint::unreachable_columns_detail(name)),
            ),
        }
    }

    /// PostgreSQL's `errorMissingRTE` hint and detail (spec/design/error-fields.md §11.2, §11.8): the
    /// first relation in the walk labeled `qualifier`, or naming that base table or view. A visible
    /// one under a different label is the table-alias hint; any other found relation gets the
    /// "cannot be referenced" detail, plus the LATERAL hint when it is hidden from a non-LATERAL
    /// subquery.
    pub(crate) fn table_hint(&self, qualifier: &str) -> (Option<String>, Option<String>) {
        let entries = self.hint_walk();
        let Some(e) = entries
            .iter()
            .find(|e| e.label == qualifier || e.catalog.as_deref() == Some(qualifier))
        else {
            return (None, None);
        };
        if e.visible && e.label != qualifier {
            return (Some(error_hint::alias_hint(&e.label)), None);
        }
        let hint = (!e.visible && e.lateral_ok).then(|| error_hint::LATERAL_TABLE.to_string());
        (hint, Some(error_hint::unreachable_table_detail(&e.label)))
    }

    /// The column at a flat index in THIS scope (the index is known valid — resolution made it).
    pub(crate) fn column_at(&self, flat: usize) -> &Column {
        for r in &self.rels {
            let n = r.table.columns.len();
            if flat >= r.offset && flat < r.offset + n {
                return &r.table.columns[flat - r.offset];
            }
        }
        unreachable!("a resolved flat column index is always in range")
    }

    /// The flat column count of this scope (the input-row width). The window base offset: a window
    /// query appends each window function's result after the input columns (spec/design/window.md
    /// §5.1), so window slot = `width() + window_index`.
    pub(crate) fn width(&self) -> usize {
        self.rels.iter().map(|r| r.table.columns.len()).sum()
    }

    /// The ancestor scope `level` hops outward (1 = immediate parent).
    pub(crate) fn ancestor(&self, level: usize) -> &Scope<'a> {
        let mut s = self;
        for _ in 0..level {
            s = s
                .parent
                .expect("a correlated level is within the scope-chain depth");
        }
        s
    }

    /// The column a resolution refers to — `Local` in this scope, or `Outer` in an ancestor.
    pub(crate) fn column_of(&self, r: Resolved) -> &Column {
        match r {
            Resolved::Local(idx) => self.column_at(idx),
            Resolved::Outer { level, index } => self.ancestor(level).column_at(index),
            Resolved::Layer { level, rel, col } => {
                &self.ancestor(level).rels[rel]
                    .layer
                    .expect("a layered relation")
                    .columns[col]
            }
        }
    }
}

/// Lift a parent-scope resolution into the child's frame: a `Local` in the parent is one hop
/// out (`level` 1); an `Outer` in the parent is one deeper hop (`level + 1`).
pub(crate) fn outer_of(r: Resolved) -> Resolved {
    match r {
        Resolved::Local(index) => Resolved::Outer { level: 1, index },
        Resolved::Outer { level, index } => Resolved::Outer {
            level: level + 1,
            index,
        },
        Resolved::Layer { level, rel, col } => Resolved::Layer {
            level: level + 1,
            rel,
            col,
        },
    }
}
