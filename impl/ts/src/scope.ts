import type { TimeZoneDeps } from "./catalog.ts";
import type { Environment } from "./environment.ts";
import type { InputZone } from "./timezone.ts";
import { noteIndexZone } from "./index_dependencies.ts";
import { noteParamType } from "./plan_params.ts";
import type {
  BoundTerm,
  CteBinding,
  CteMode,
  MergeCol,
  Outcome,
  PlanJoin,
  QueryPlan,
  RExpr,
  Resolved,
  ResolvedType,
  ScopeRel,
  SelectPlan,
} from "./executor.ts";
import {
  type Engine,
  queryPlanColumnDerivs,
  queryPlanColumnTypmods,
  astSubscriptExprs,
  exprHasAggregate,
  itemsHaveAggregate,
  outerOf,
} from "./executor.ts";
import type { Column, SeqDataType, SeqOwner, SequenceDef, Table } from "./catalog.ts";
import {
  type Deriv,
  type Typmod,
  TYPMOD_NONE,
  commonCollation,
  commonTypmod,
  typmodEqual,
} from "./resolve.ts";
import {
  columnIndex,
  seqDataTypeDefaultBounds,
  seqDataTypeFromName,
  seqDataTypePgName,
  seqDataTypeRange,
} from "./catalog.ts";
import { EngineError, engineError } from "./errors.ts";
import {
  FuzzyColumnMatch,
  LATERAL_COLUMN,
  LATERAL_TABLE,
  QUALIFY_COLUMN,
  RECURSIVE_TERM_COLLATION,
  RECURSIVE_TERM_TYPE,
  TRY_QUALIFIED,
  aliasHint,
  unreachableColumnDetail,
  unreachableColumnsDetail,
  unreachableTableDetail,
} from "./error_hint.ts";
import type { DecimalTypmod, ScalarType, Type } from "./types.ts";
import {
  arrayT,
  rangeT,
  canonicalName,
  compositeT,
  recordT,
  isBool,
  isBytea,
  isDate,
  isDecimal,
  isFloat,
  isInteger,
  isInterval,
  isJson,
  isJsonPath,
  isJsonb,
  isText,
  isTimestamp,
  isTimestamptz,
  isUuid,
  rank,
  scalarT,
} from "./types.ts";
import { rangeNameForElement } from "./range.ts";
import type {
  BinaryOp,
  Cte,
  CteBody,
  Delete,
  Expr,
  Insert,
  JsonOnBehavior,
  JsonWrapper,
  OrderKey,
  QueryExpr,
  ReturningClause,
  Select,
  SeqOptions,
  SeqRestart,
  SetOp,
  SetOpKind,
  Statement,
  TableRef,
  Update,
  WindowDef,
  WithQuery,
} from "./ast.ts";
import { cteBodyAsQuery, cteBodyIsDataModifying, forEachGroupExpr } from "./ast.ts";
import { outputColumnLocation, requireSetopEquality, unifySetopColumn } from "./eval_ops.ts";
import type { Value } from "./value.ts";
import { render, renderFloatPg } from "./value.ts";
import {
  coerceForStore,
  coerceRecordTextToValue,
  coerceStringToArray,
  storeValue,
} from "./store.ts";
import { coerceStringToRange } from "./kernels.ts";
import { finalizeRange, rangeForElement } from "./range.ts";
import type { RangeDesc } from "./ranges_gen.ts";
import { type ColType, colTypeScalar } from "./catalog.ts";
import { quoteIdentifier } from "./parser.ts";
import type { Privilege } from "./privileges.ts";
import type { TableStore } from "./storage.ts";
import type { ViewWrite, WriteLayer } from "./view_write.ts";
// SubRel is one inner relation label of an unaliased parenthesized join (grammar.md §15): its
// table's columns live at the flat slots `slots` of the join's single FROM relation (slot k holds
// column k). rel is the owning FROM ordinal, so a partial ON scope sees only the labels of the
// relations it spans.
export type SubRel = {
  label: string;
  table: Table;
  slots: number[];
  rel: number;
};

// subRelsWithin returns the sub-relation labels owned by FROM ordinals lo..hi inclusive.
export function subRelsWithin(subs: SubRel[], lo: number, hi: number): SubRel[] {
  return subs.filter((sr) => sr.rel >= lo && sr.rel <= hi);
}

// UnseenRel is a relation of a query level's FROM clause that a reference there cannot see.
// PostgreSQL's range table holds such relations, and its 42703 / 42P01 hints search them
// (spec/design/error-fields.md §11.8).
export type UnseenRel = {
  label: string;
  columns: string[];
  // The base table or view the relation names (the §11.2 search), else undefined.
  catalog: string | undefined;
  // Marking the reference's subquery LATERAL would make the relation visible.
  lateralOk: boolean;
  // The relation is an inner relation of a completed unaliased join, so its columns are reachable
  // only through its qualifier.
  qualifiedOnly: boolean;
};

// UnseenFrame is the relations to the left of a non-LATERAL derived table or parenthesized join,
// recorded while its body is planned (spec/design/error-fields.md §11.8). depth is the scope-chain
// depth of the scope the body is planned against, so the hint walk visits the relations right after
// the body's own outermost level. parenJoin marks a parenthesized join's body: its ON clauses cannot
// reach these relations even with LATERAL.
export type UnseenFrame = {
  depth: number;
  parenJoin: boolean;
  rels: UnseenRel[];
};

// ScopeHints is a scope's hint-only facts (spec/design/error-fields.md §11.8). They never affect
// resolution.
export type ScopeHints = {
  // This level's FROM relations the scope cannot see, before its own: a JOIN's ON clause cannot see
  // the comma items to its left.
  unseen: UnseenRel[];
  // The scope resolves a JOIN's ON clause.
  onClause: boolean;
  // The scope resolves a table function's arguments (PostgreSQL's p_lateral_active): a hint never
  // suggests LATERAL from here.
  lateralActive: boolean;
};

// NO_SCOPE_HINTS is the hint facts of an ordinary scope. Shared, never mutated.
const NO_SCOPE_HINTS: ScopeHints = { unseen: [], onClause: false, lateralActive: false };

// HintEntry is one relation the hint walk visits (spec/design/error-fields.md §11.8), in walk order.
type HintEntry = {
  label: string;
  columns: string[];
  catalog: string | undefined;
  visible: boolean;
  lateralOk: boolean;
  qualifiedOnly: boolean;
};

// scopeDepth is the number of levels in a scope chain (0 for none).
export function scopeDepth(scope: Scope | null): number {
  let depth = 0;
  for (let s = scope; s !== null; s = s.parent) depth++;
  return depth;
}

export class Scope {
  rels: ScopeRel[];
  // parent is the enclosing query's scope, for correlated resolution (null at top level).
  parent: Scope | null;
  // catalog lets a subquery's inner FROM tables be looked up during planning.
  catalog: Engine;
  // allowSubquery is true inside a SELECT (and its nested subqueries), false for UPDATE/DELETE
  // (a subquery there is 0A000 this slice).
  allowSubquery: boolean;
  // The statement's CTE bindings visible here (spec/design/cte.md §2). Inherited DIRECTLY down into
  // nested scopes (a subquery sees the same `ctes`), NOT via the `parent` chain — so CTE lookup
  // never counts as a correlation level. Empty for every non-WITH statement.
  ctes: CteBinding[];
  // USING/NATURAL merged columns (spec/design/grammar.md §15) — a bare reference to a merge name
  // resolves to its index (checked before the per-relation search, so it is never the underlying
  // copies' 42702 ambiguity). Empty except in a SELECT whose FROM has a USING join.
  merges: MergeCol[] = [];
  // Flat indices SUPERSEDED by a merge — the underlying left+right copies, omitted from `*`
  // expansion (still reachable qualified) — plus the copies an unaliased parenthesized join carries
  // after its star columns. Empty unless the FROM has a USING/NATURAL join.
  hidden: number[] = [];
  // The relation labels an UNALIASED parenthesized join keeps visible (spec/design/grammar.md §15):
  // `(a JOIN b ON …)` is one FROM relation, yet `a.x` and `b.*` still name its inner relations.
  // Reached only by qualified references; never physical.
  subRels: SubRel[] = [];
  // The flat indices bare `*` expands to, in order (grammar.md §15): PostgreSQL's per-join layout —
  // each USING/NATURAL join's merged columns, then its left input's remaining columns, then its
  // right input's — concatenated across comma items. null for a scope the FROM-join planner did not
  // build (then `*` is every non-hidden column in FROM order).
  star: number[] | null = null;
  // A PERSISTED index expression's scope (an expression key or a partial-index predicate —
  // spec/design/collation.md §1 "Function results"): collation derivation keeps the storage rule (a
  // column, COLLATE, ||; a function result carries none), so stored key bytes and partial-index
  // membership never change meaning under the query-side function propagation.
  storageCollation = false;
  // Each scalar subquery resolved against this scope, keyed by its AST node, with its output
  // column's collation derivation (spec/design/collation.md §1 "Scalar subqueries") and typmod
  // (spec/design/create-table-as.md §2). resolve records them when it plans the body, so
  // deriveCollation / deriveTypmod — which work on the AST — can read them without planning the body
  // again. Keyed by identity: a lookup always follows the resolve of the same node, which overwrites
  // any stale entry.
  subqueryDerivs: Map<QueryExpr, { deriv: Deriv; typmod: Typmod }> = new Map();
  // Set on a LATERAL FENCE only (spec/design/grammar.md §15): the empty scope a parenthesized join's
  // body is planned against when the join is not the first item of its FROM. The fence hides the
  // relations to the join's left from the body's ON clauses and non-LATERAL items (PostgreSQL's
  // lateral-only namespace entries), while this view — the prefix of those relations, at the same
  // correlation level — is what the body's LATERAL items and table functions resolve against. Only a
  // parenthesized-join body reads it; any other SELECT planned against a fence strips it.
  lateralView: Scope | null = null;
  // Set when a column reference resolves to one of THIS scope's relations. Read on a parenthesized
  // join's lateral prefix to learn whether the join's body correlates to the relations on its left
  // (grammar.md §15).
  lateralHit = false;
  // Set on a LATERAL prefix whose relations are the left side of a RIGHT or FULL join (PostgreSQL's
  // p_lateral_ok = false, spec/design/grammar.md §44): a reference that resolves to one of them is
  // 42P10. A reference that passes them to an outer level is unaffected.
  lateralRefused = false;
  // Set on a LATERAL prefix inside a parenthesized join's body (spec/design/grammar.md §15): its
  // parent — the fence's view — holds relations PostgreSQL places in the same FROM namespace, so a
  // bare column matching in both is ambiguous (42702), not shadowed.
  sharesParentLevel = false;
  // What a name-resolution hint needs beyond the visible relations (error-fields.md §11.8).
  hints: ScopeHints = NO_SCOPE_HINTS;
  constructor(
    rels: ScopeRel[],
    catalog: Engine,
    parent: Scope | null,
    allowSubquery: boolean,
    ctes: CteBinding[] = [],
  ) {
    this.rels = rels;
    this.catalog = catalog;
    this.parent = parent;
    this.allowSubquery = allowSubquery;
    this.ctes = ctes;
  }

  // lateralFence is the fence a parenthesized join's body is planned against (spec/design/grammar.md
  // §15): an empty level over parent, so the body's ON clauses and non-LATERAL items see no relation
  // to the join's left, carrying view — the prefix of those relations, one scope at the same
  // correlation level — for the body's LATERAL items and table functions.
  static lateralFence(
    parent: Scope | null,
    view: Scope,
    catalog: Engine,
    ctes: CteBinding[],
  ): Scope {
    const fence = new Scope([], catalog, parent, true, ctes);
    fence.lateralView = view;
    return fence;
  }

  // withoutLateralView is this fence without its lateral view: the same empty level, read by a SELECT
  // that is not the parenthesized join the fence was built for (§15).
  withoutLateralView(): Scope {
    return new Scope([], this.catalog, this.parent, true, this.ctes);
  }

  // single builds a one-relation scope with no parent (the single-table UPDATE / DELETE case).
  // Subqueries ARE allowed: a correlated reference resolves to the target row via the per-row
  // outer environment (the subquery's parent is this scope), an uncorrelated one folds once
  // (spec/design/grammar.md §26). SELECT builds its own scope in planSelect.
  static single(catalog: Engine, t: Table): Scope {
    return new Scope([{ label: t.name, table: t, offset: 0 }], catalog, null, true);
  }

  // writeTarget is the target scope of a write (UPDATE / DELETE): single, or, for a write through a
  // view, the base table relabeled with the view's name and exposing the view's columns
  // (spec/design/views.md §15.2).
  static writeTarget(catalog: Engine, t: Table, vw: ViewWrite | null): Scope {
    const scope = Scope.single(catalog, t);
    if (vw !== null) {
      scope.rels[0]!.label = vw.view;
      scope.rels[0]!.layer = vw.top;
    }
    return scope;
  }

  // layerBody is the isolated scope a view body's select items and WHERE resolve in during a write
  // through the view (views.md §4.1, §15.2): the body's single FROM item, labeled `label`, exposing
  // `layer`'s columns over the base row at flat `offset`. No parent and no CTEs.
  static layerBody(
    catalog: Engine,
    t: Table,
    offset: number,
    label: string,
    layer: WriteLayer,
  ): Scope {
    return new Scope([{ label, table: t, offset, layer }], catalog, null, true);
  }

  // index is the scope a PERSISTED index expression (an expression key or a partial-index predicate)
  // resolves against: single with the storage collation rule (storageCollation).
  static index(catalog: Engine, t: Table): Scope {
    const scope = Scope.single(catalog, t);
    scope.storageCollation = true;
    return scope;
  }

  // empty is the column-less scope a DEFAULT expression resolves against (constraints.md §2): a
  // default may not reference a column (rejected as 0A000 by the structural pre-walk before
  // resolution) and may not contain a subquery, so there are no relations and subqueries are
  // disallowed.
  static empty(catalog: Engine): Scope {
    return new Scope([], catalog, null, false);
  }

  // returning is the scope a RETURNING list resolves against (grammar.md §32): the target
  // table at offset 0 (bare and table-qualified references read the BASE row), plus the
  // old/new row-version pseudo-relations as QUALIFIER-ONLY rels over the concatenated
  // projection row [base | other]. baseIsOld says which version the base row is: false for
  // INSERT/UPDATE (base = the new row, `old` reads the other half), true for DELETE (base =
  // the old row, `new` reads the other half) — the absent version is the all-NULL row the
  // caller appends. Explicit WITH (OLD AS o, NEW AS n) aliases are installed first and hide that
  // version's standard name; an unaliased default is suppressed when its label is already occupied
  // (including by the target table). Explicit aliases may not collide with the target or each other
  // (42712). An INSERT's AS alias (upsert.md §5) replaces the target's label, so the table (or view)
  // name is no longer a visible qualifier — referencing it is 42P01 with the table-alias hint
  // (error-fields.md §11.2).
  static returning(
    catalog: Engine,
    t: Table,
    baseIsOld: boolean,
    returning: ReturningClause,
    vw: ViewWrite | null = null,
    alias?: string,
  ): Scope {
    const n = t.columns.length;
    const layer = vw === null ? {} : { layer: vw.top };
    const oldOffset = baseIsOld ? 0 : n;
    const newOffset = baseIsOld ? n : 0;
    const rels: ScopeRel[] = [Scope.targetRel(t, vw, alias)];
    for (const explicit of [
      { alias: returning.oldAlias, offset: oldOffset },
      { alias: returning.newAlias, offset: newOffset },
    ]) {
      if (explicit.alias !== null) {
        const alias = explicit.alias;
        if (rels.some((r) => r.label === alias)) {
          throw engineError("duplicate_alias", `table name ${alias} specified more than once`);
        }
        rels.push({
          label: alias,
          table: t,
          offset: explicit.offset,
          qualifierOnly: true,
          ...layer,
        });
      }
    }
    for (const pseudo of [
      { label: "old", offset: oldOffset, aliased: returning.oldAlias !== null },
      { label: "new", offset: newOffset, aliased: returning.newAlias !== null },
    ]) {
      if (!pseudo.aliased && !rels.some((r) => r.label === pseudo.label)) {
        rels.push({
          label: pseudo.label,
          table: t,
          offset: pseudo.offset,
          qualifierOnly: true,
          ...layer,
        });
      }
    }
    return new Scope(rels, catalog, null, true);
  }

  // targetRel is the target relation of an INSERT's RETURNING / DO UPDATE scope (or an
  // UPDATE/DELETE's RETURNING) at offset 0: labeled by the INSERT's AS alias when given (upsert.md
  // §5), else by the view's name for a write through a view (views.md §15.2), else by the table's
  // name. The catalog name is recorded (base / view) so a reference by the hidden table or view name
  // gets the table-alias hint (error-fields.md §11.2).
  static targetRel(t: Table, vw: ViewWrite | null, alias: string | undefined): ScopeRel {
    const rel: ScopeRel =
      vw === null
        ? { label: t.name, table: t, offset: 0, base: true }
        : { label: vw.view, table: t, offset: 0, view: vw.view, layer: vw.top };
    if (alias !== undefined) rel.label = alias;
    return rel;
  }

  // onConflictExcluded is the scope a DO UPDATE's SET/WHERE resolve against
  // (spec/design/upsert.md §5): the target table at offset 0 — labeled by the INSERT's AS alias when
  // given — whose bare and label-qualified references read the EXISTING conflicting row, plus
  // `excluded` as a QUALIFIER-ONLY relation at offset n over the combined row [existing | proposed]
  // (excluded.col reads the proposed row). A target labeled `excluded` (a table so named, or AS
  // excluded) keeps BOTH relations under the one label, so excluded.col is 42P09 (PostgreSQL's rule
  // — resolveQualified). For an INSERT through a view (vw, views.md §15.7) both relations expose the
  // view's columns over their halves of the combined row.
  static onConflictExcluded(
    catalog: Engine,
    t: Table,
    vw: ViewWrite | null,
    alias?: string,
  ): Scope {
    const n = t.columns.length;
    const rels: ScopeRel[] = [
      Scope.targetRel(t, vw, alias),
      {
        label: "excluded",
        table: t,
        offset: n,
        qualifierOnly: true,
        ...(vw === null ? {} : { layer: vw.top }),
      },
    ];
    return new Scope(rels, catalog, null, true);
  }

  // resolveBare resolves a bare name in expression position: a column (resolveBareColumn) when any
  // scope in the chain has one, else a WHOLE-ROW reference to a scalar function relation of that
  // label (resolveScalarFuncRow) — PostgreSQL's transformColumnRef order, which tries every level
  // for a column before trying the name as a relation. 42703 when neither matches.
  resolveBare(name: string): Resolved {
    try {
      return this.resolveBareColumn(name);
    } catch (e) {
      if (!(e instanceof EngineError) || e.state !== "undefined_column") throw e;
      const r = this.resolveScalarFuncRow(name);
      if (r === null) throw e;
      return r;
    }
  }

  // hit records a reference resolved to this scope's relation `label` (lateralHit), or refuses it
  // when this scope is the left side of a RIGHT/FULL join — PostgreSQL's check_lateral_ref_ok, raised
  // at the first matching relation (grammar.md §44).
  private hit(label: string): void {
    this.refuse(label);
    this.lateralHit = true;
  }

  // refuse is 42P10 when this scope is the left side of a RIGHT/FULL join (hit).
  private refuse(label: string): void {
    if (this.lateralRefused) {
      throw engineError(
        "invalid_column_reference",
        "invalid reference to FROM-clause entry for table " + label,
      ).withDetail("The combining JOIN type must be INNER or LEFT for a LATERAL reference.");
    }
  }

  // parentLevelHasColumn reports whether the fence views PostgreSQL places in this scope's FROM
  // namespace (sharesParentLevel, grammar.md §15) have a visible column `name`, raising that
  // namespace's errors in its order — outermost first: a second match is 42702, a refused first
  // match 42P10.
  private parentLevelHasColumn(name: string): boolean {
    if (!this.sharesParentLevel || this.parent === null) return false;
    return this.parent.levelHasColumn(name);
  }

  // levelHasColumn is parentLevelHasColumn, then this scope's own columns.
  private levelHasColumn(name: string): boolean {
    let seen = this.parentLevelHasColumn(name);
    const matched = (label: string): void => {
      if (seen) throw ambiguousColumn(name);
      this.refuse(label);
      seen = true;
    };
    if (this.merges.some((m) => m.name === name)) matched("unnamed_join");
    for (const r of this.rels) {
      if (r.qualifierOnly) continue;
      if (r.layer !== undefined) {
        for (const c of r.layer.columns) if (c.name === name) matched(r.label);
        continue;
      }
      for (let local = 0; local < r.table.columns.length; local++) {
        const idx = r.offset + local;
        if (r.table.columns[local]!.name === name && !this.hidden.includes(idx)) {
          matched(this.refusedLabel(r, idx));
        }
      }
    }
    return seen;
  }

  // refusedLabel is the label PostgreSQL names a relation by when a bare column idx of this scope is
  // refused (hit): its label, else unnamed_join for an unaliased parenthesized join's column, else
  // unnamed_subquery (an unaliased derived table).
  private refusedLabel(r: ScopeRel, idx: number): string {
    if (r.label !== "") return r.label;
    if (this.subRels.some((sr) => sr.slots.includes(idx))) return "unnamed_join";
    return "unnamed_subquery";
  }

  // resolveScalarFuncRow resolves a bare name that labels a scalar set-returning function's relation
  // as a whole-row reference to it (spec/design/functions.md §10): a scalar function's row IS its
  // one column, so `SELECT e FROM jsonb_array_elements(…) AS e` reads the element (PostgreSQL's
  // makeWholeRowVar for a scalar function). Searched THIS scope, then outward (nearest wins). No
  // other relation kind has a whole-row value in jed (no implicit per-table row types).
  resolveScalarFuncRow(name: string): Resolved | null {
    for (const r of this.rels) {
      if (
        r.qualifierOnly ||
        r.layer !== undefined ||
        r.label !== name ||
        r.table.scalarFunc !== true
      ) {
        continue;
      }
      this.hit(name);
      return { level: 0, index: r.offset };
    }
    // An unaliased parenthesized join keeps its inner function relation's label visible.
    for (const sr of this.subRels) {
      if (sr.label === name && sr.table.scalarFunc === true) {
        this.hit(name);
        return { level: 0, index: sr.slots[0]! };
      }
    }
    if (this.parent === null) return null;
    const r = this.parent.resolveScalarFuncRow(name);
    return r === null ? null : outerOf(r);
  }

  // resolveBareColumn resolves a bare column name against THIS scope, then OUTWARD through the
  // parent chain. Within one scope: two+ relations have it → 42702 ambiguous; exactly one → local;
  // none → fall through to the parent. A name found only in an ancestor is an outer reference
  // (nearest scope wins). 42703 only if no scope in the chain has it.
  // A qualifier-only rel (the RETURNING old/new pseudo-relations) is invisible here — no
  // new ambiguity (grammar.md §32). A column only, never a whole-row reference: a USING column
  // resolves through this directly.
  resolveBareColumn(name: string): Resolved {
    const lower = name;
    // A USING/NATURAL MERGE column resolves to its surviving side (grammar.md §15), seeded here so
    // the bare name binds the merged column rather than its two (hidden) underlying copies — which is
    // why such a join column is unambiguous. A non-hidden column elsewhere with the same name still
    // makes the reference ambiguous (a third relation sharing the name).
    // A match in the fence views of the same FROM namespace comes first (§15).
    const levelSeen = this.parentLevelHasColumn(name);
    let found = -1;
    for (const m of this.merges) {
      if (m.name === lower) {
        found = m.index;
        break;
      }
    }
    if (found >= 0) {
      if (levelSeen) throw ambiguousColumn(name);
      this.hit("unnamed_join");
    }
    let layered: Resolved | null = null;
    for (let ri = 0; ri < this.rels.length; ri++) {
      const r = this.rels[ri]!;
      if (r.qualifierOnly) continue;
      // A write through a view exposes the view's columns (views.md §15.2).
      if (r.layer !== undefined) {
        for (let col = 0; col < r.layer.columns.length; col++) {
          if (r.layer.columns[col]!.name !== lower) continue;
          if (levelSeen || found >= 0 || layered !== null) throw ambiguousColumn(name);
          this.hit(r.label);
          layered = { level: 0, index: -1, layer: { rel: ri, col } };
        }
        continue;
      }
      // Count EVERY matching column, not just the first per relation: a synthetic relation (a CTE or
      // derived table) may carry two columns of the same name, and a bare reference to that name is
      // ambiguous (42702) exactly as a match across two relations is (cte.md §2, grammar.md §42).
      // Base tables have unique column names, so this only fires for a duplicate-output-name relation.
      for (let local = 0; local < r.table.columns.length; local++) {
        const idx = r.offset + local;
        // A merge's underlying copies are superseded by the merge above — skip them.
        if (this.hidden.includes(idx)) continue;
        if (r.table.columns[local]!.name === lower) {
          if (levelSeen || found >= 0 || layered !== null) throw ambiguousColumn(name);
          this.hit(this.refusedLabel(r, idx));
          found = idx;
        }
      }
    }
    if (layered !== null) return layered;
    if (found >= 0) {
      this.traceFlat(found);
      return { level: 0, index: found };
    }
    if (this.parent !== null) return outerOf(this.parent.resolveBareColumn(name));
    throw undefinedColumn(name);
  }

  // starLabel is the label a frozen `*` column renders with (spec/design/views.md §5): null (a bare
  // `"column"`) for a USING/NATURAL merged column and for a relation with no label (an unaliased
  // derived table); an unaliased parenthesized join's column takes its inner relation's label; any
  // other column its relation's label.
  starLabel(idx: number): string | null {
    if (this.merges.some((m) => m.index === idx)) return null;
    for (const r of this.rels) {
      const n = r.table.columns.length;
      if (idx >= r.offset && idx < r.offset + n) {
        if (r.label !== "") return r.label;
        const sr = this.subRels.find((sub) => sub.slots.includes(idx));
        return sr === undefined ? null : sr.label;
      }
    }
    return null;
  }

  // traceFlat records a resolved column of this scope as a view dependency when a view body is being
  // traced and the column belongs to a catalog base table (spec/design/views.md §6). A no-op on every
  // ordinary statement.
  traceFlat(idx: number): void {
    if (this.catalog.viewTrace === null) return;
    for (const r of this.rels) {
      const n = r.table.columns.length;
      if (idx >= r.offset && idx < r.offset + n) {
        this.catalog.traceRelColumn(r, idx - r.offset);
        return;
      }
    }
  }

  // resolveQualified resolves a qualified rel.col against THIS scope, then outward. A qualifier
  // naming a relation here binds — a missing column is then 42703 (no fall-through). Only an
  // unknown qualifier walks outward (42P01 if no ancestor has it).
  resolveQualified(qualifier: string, name: string): Resolved {
    const q = qualifier;
    // Two relations under one label at this level — only the DO UPDATE scope of a target labeled
    // `excluded` builds one (every FROM rejects a duplicate label up front, 42712): PostgreSQL's
    // 42P09 (upsert.md §5).
    if (this.rels.filter((r) => r.label === q).length > 1) {
      throw engineError("ambiguous_alias", `table reference ${qualifier} is ambiguous`);
    }
    for (let ri = 0; ri < this.rels.length; ri++) {
      const layer = this.rels[ri]!.layer;
      if (this.rels[ri]!.label !== q || layer === undefined) continue;
      this.hit(qualifier);
      let found = -1;
      for (let col = 0; col < layer.columns.length; col++) {
        if (layer.columns[col]!.name !== name) continue;
        if (found >= 0) throw ambiguousColumn(name);
        found = col;
      }
      if (found < 0) throw undefinedColumn(name);
      return { level: 0, index: -1, layer: { rel: ri, col: found } };
    }
    for (const r of this.rels) {
      if (r.label === q) {
        // PostgreSQL refuses the relation itself, before looking up the column.
        this.hit(qualifier);
        // A synthetic relation (a derived table, an aliased parenthesized join) may carry two
        // columns of one name; a qualified reference to it is ambiguous (42702) exactly as a bare
        // one is (PG). A USING/NATURAL copy hidden by a merge yields to the visible column.
        const lowerName = name;
        let found = -1;
        let visible = -1;
        let ambiguous = false;
        for (let local = 0; local < r.table.columns.length; local++) {
          if (r.table.columns[local]!.name !== lowerName) continue;
          const idx = r.offset + local;
          if (found < 0) found = idx;
          if (!this.hidden.includes(idx)) {
            ambiguous = ambiguous || visible >= 0;
            visible = idx;
          }
        }
        if (found < 0) throw undefinedColumn(name);
        if (ambiguous) throw ambiguousColumn(name);
        const idx = visible >= 0 ? visible : found;
        this.traceFlat(idx);
        return { level: 0, index: idx };
      }
    }
    for (const sr of this.subRels) {
      if (sr.label === q) {
        this.hit(qualifier);
        const local = columnIndex(sr.table, name);
        if (local < 0) throw undefinedColumn(name);
        return { level: 0, index: sr.slots[local]! };
      }
    }
    if (this.parent !== null) return outerOf(this.parent.resolveQualified(qualifier, name));
    throw missingFromEntry(qualifier);
  }

  // resolveBareAt is resolveBare for a column reference written at `loc`: a name-resolution error
  // carries the reference's position and its hint (spec/design/error-fields.md §10.2, §11).
  resolveBareAt(name: string, loc: number | undefined): Resolved {
    try {
      return this.resolveBare(name);
    } catch (e) {
      throw this.locateColumnError(e, null, name, loc);
    }
  }

  // resolveQualifiedAt is resolveQualified for a reference written at `loc` (§10.2, §11).
  resolveQualifiedAt(qualifier: string, name: string, loc: number | undefined): Resolved {
    try {
      return this.resolveQualified(qualifier, name);
    } catch (e) {
      throw this.locateColumnError(e, qualifier, name, loc);
    }
  }

  // locateColumnError stamps a column reference's 42703 / 42702 / 42P01 / 42P09 / 42P10 with its position, and adds
  // the misspelled-column (§11.1), table-alias (§11.2), or LATERAL (§11.8) hint. An error already
  // positioned came from an inner site, which wins. Returns the (possibly annotated) error for the
  // caller to rethrow.
  private locateColumnError(
    e: unknown,
    qualifier: string | null,
    name: string,
    loc: number | undefined,
  ): unknown {
    if (!(e instanceof EngineError) || e.position !== undefined) return e;
    const code = e.code();
    if (
      code !== "42703" &&
      code !== "42702" &&
      code !== "42P01" &&
      code !== "42P09" &&
      code !== "42P10"
    ) {
      return e;
    }
    let hd: [string | undefined, string | undefined] = [undefined, undefined];
    if (code === "42703") hd = this.columnHint(qualifier, name);
    else if (code === "42P01" && qualifier !== null) hd = this.tableHint(qualifier);
    e.withPosition(loc).withHint(hd[0]);
    if (e.detail === undefined) e.withDetail(hd[1]);
    return e;
  }

  // hintRelations is the labeled relations this level can see, in the §11.1 walk order: the FROM
  // relations in order, then the inner relations of an unaliased parenthesized join. Each yields its
  // label, column names (a write through a view: the view's columns), and the base table or view it
  // names (§11.2).
  private hintRelations(): { label: string; cols: string[]; catalog: string | undefined }[] {
    const out: { label: string; cols: string[]; catalog: string | undefined }[] = [];
    for (const r of this.rels) {
      if (r.label === "") continue;
      const cols = r.layer !== undefined ? r.layer.columns : r.table.columns;
      const catalog = r.view !== undefined ? r.view : r.base === true ? r.table.name : undefined;
      out.push({ label: r.label, cols: cols.map((c) => c.name), catalog });
    }
    for (const sr of this.subRels) {
      if (sr.label === "") continue;
      out.push({ label: sr.label, cols: sr.table.columns.map((c) => c.name), catalog: undefined });
    }
    return out;
  }

  // depth is the scope-chain depth of this scope (1 for a scope with no parent).
  depth(): number {
    return scopeDepth(this);
  }

  // hintWalk is every relation a hint searches, in PostgreSQL's range-table walk order
  // (spec/design/error-fields.md §11.1, §11.8): at each level outward, the relations this level
  // cannot see to the left of an ON clause, the visible relations, then the relations hidden from the
  // derived bodies planned against the level's parent (innermost first).
  private hintWalk(): HintEntry[] {
    const frames = this.catalog.unseenFrames;
    const lateralActive = this.hints.lateralActive;
    const unseen = (u: UnseenRel, blocked: boolean): HintEntry => ({
      label: u.label,
      columns: u.columns,
      catalog: u.catalog,
      visible: false,
      lateralOk: u.lateralOk && !blocked && !lateralActive,
      qualifiedOnly: u.qualifiedOnly && !blocked,
    });
    const out: HintEntry[] = [];
    let depth = this.depth();
    for (let s: Scope | null = this; s !== null; s = s.parent) {
      for (const u of s.hints.unseen) out.push(unseen(u, true));
      for (const { label, cols, catalog } of s.hintRelations()) {
        out.push({
          label,
          columns: cols,
          catalog,
          visible: true,
          lateralOk: false,
          qualifiedOnly: false,
        });
      }
      depth--;
      for (let fi = frames.length - 1; fi >= 0; fi--) {
        const f = frames[fi]!;
        if (f.depth !== depth) continue;
        // A parenthesized join's ON clause cannot see the relations to the join's left.
        const blocked = s.hints.onClause && f.parenJoin;
        for (const u of f.rels) out.push(unseen(u, blocked));
      }
    }
    return out;
  }

  // columnHint is PostgreSQL's errorMissingColumn [hint, detail] (spec/design/error-fields.md §11.1,
  // §11.8): a relation that has the column but cannot be seen from here yields the "cannot be
  // referenced" detail and the LATERAL or qualified-name hint; otherwise the misspelled-column
  // suggestion.
  private columnHint(
    qualifier: string | null,
    name: string,
  ): [string | undefined, string | undefined] {
    const entries = this.hintWalk();
    const m = new FuzzyColumnMatch(qualifier, name);
    const exact: HintEntry[] = [];
    for (const e of entries) {
      const penalty = m.penalty(e.label);
      for (const col of e.columns) m.offer(e.label, penalty, col);
      if (penalty === 0 && e.columns.includes(name)) exact.push(e);
    }
    if (exact.length === 0) return [m.hint(), undefined];
    if (exact.length === 1) {
      const e = exact[0]!;
      let hint: string | undefined;
      if (e.lateralOk) hint = LATERAL_COLUMN;
      else if (qualifier === null && e.qualifiedOnly) hint = QUALIFY_COLUMN;
      return [hint, unreachableColumnDetail(name, e.label)];
    }
    return [qualifier === null ? TRY_QUALIFIED : undefined, unreachableColumnsDetail(name)];
  }

  // tableHint is PostgreSQL's errorMissingRTE [hint, detail] (spec/design/error-fields.md §11.2,
  // §11.8): the first relation in the walk labeled `qualifier`, or naming that base table or view. A
  // visible one under a different label is the table-alias hint; any other found relation gets the
  // "cannot be referenced" detail, plus the LATERAL hint when it is hidden from a non-LATERAL
  // subquery.
  private tableHint(qualifier: string): [string | undefined, string | undefined] {
    const e = this.hintWalk().find((h) => h.label === qualifier || h.catalog === qualifier);
    if (e === undefined) return [undefined, undefined];
    if (e.visible && e.label !== qualifier) return [aliasHint(e.label), undefined];
    const hint = !e.visible && e.lateralOk ? LATERAL_TABLE : undefined;
    return [hint, unreachableTableDetail(e.label)];
  }

  // columnAt returns the column at a flat index in THIS scope (index known valid).
  columnAt(flat: number): Column {
    for (const r of this.rels) {
      const n = r.table.columns.length;
      if (flat >= r.offset && flat < r.offset + n) return r.table.columns[flat - r.offset]!;
    }
    throw new Error("a resolved flat column index is always in range");
  }

  // ancestor returns the scope `level` hops outward (1 = immediate parent).
  ancestor(level: number): Scope {
    let s: Scope = this;
    for (let i = 0; i < level; i++) s = s.parent!;
    return s;
  }

  // columnOf returns the column a resolution refers to — local here, or outer in an ancestor.
  columnOf(r: Resolved): Column {
    if (r.layer !== undefined) {
      return this.ancestor(r.level).rels[r.layer.rel]!.layer!.columns[r.layer.col]!;
    }
    return this.ancestor(r.level).columnAt(r.index);
  }

  // width returns the flat column count of THIS scope (the input-row width). It is the window base
  // offset: a window query appends each window function's result after the input columns
  // (spec/design/window.md §5.1), so window slot = width() + windowIndex.
  width(): number {
    return this.rels.reduce((sum, r) => sum + r.table.columns.length, 0);
  }
}

// undefinedColumn is 42703 — a column name that no relation in scope defines.
export function undefinedColumn(name: string): EngineError {
  return engineError("undefined_column", "column does not exist: " + name);
}

// ambiguousColumn is 42702 — a bare column name that more than one relation in scope defines.
export function ambiguousColumn(name: string): EngineError {
  return engineError("ambiguous_column", "column reference " + name + " is ambiguous");
}

// missingFromEntry is 42P01 — a qualifier that names no relation in the FROM clause.
export function missingFromEntry(qualifier: string): EngineError {
  return engineError("undefined_table", "missing FROM-clause entry for table " + qualifier);
}

// resolvedTypeOf is the resolved (static) type of a column of scalar type ty.
export function resolvedTypeOf(ty: ScalarType): ResolvedType {
  if (isText(ty)) return { kind: "text" };
  if (isBool(ty)) return { kind: "bool" };
  if (isDecimal(ty)) return { kind: "decimal" };
  if (isFloat(ty)) return { kind: "float", ty };
  if (isBytea(ty)) return { kind: "bytea" };
  if (isUuid(ty)) return { kind: "uuid" };
  if (isTimestamp(ty)) return { kind: "timestamp" };
  if (isTimestamptz(ty)) return { kind: "timestamptz" };
  if (isDate(ty)) return { kind: "date" };
  if (isInterval(ty)) return { kind: "interval" };
  if (isJson(ty)) return { kind: "json" };
  if (isJsonb(ty)) return { kind: "jsonb" };
  if (isJsonPath(ty)) return { kind: "jsonpath" };
  return { kind: "int", ty };
}

// resolvedTypeOfCol is the resolved (static) type of a column of catalog type `ty` — a scalar via
// resolvedTypeOf, or a composite resolved to a CompositeRType (its name + the resolved field types,
// recursing) against the database's composite-type catalog (spec/design/composite.md §5). The
// composite reference is guaranteed to resolve (CREATE TYPE / the two-pass load validated it).
export function resolvedTypeOfCol(ty: Type, db: Engine): ResolvedType {
  if (ty.kind === "scalar") return resolvedTypeOf(ty.scalar);
  if (ty.kind === "array") return { kind: "array", elem: resolvedTypeOfCol(ty.elem, db) };
  if (ty.kind === "range") return { kind: "range", elem: resolvedTypeOfCol(ty.elem, db) };
  if (ty.kind === "record") {
    // An anonymous record round-trips to the nameless composite a ROW(…) resolves to.
    return {
      kind: "composite",
      name: null,
      fields: ty.fields.map((f) => ({ name: f.name, type: resolvedTypeOfCol(f.ty, db) })),
    };
  }
  const def = db.compositeType(ty.name);
  if (def === undefined) throw new Error("composite type reference resolved at load / CREATE TYPE");
  return {
    kind: "composite",
    name: def.name,
    fields: def.fields.map((f) => ({
      name: f.name,
      type: resolvedTypeOfCol(f.type, db),
    })),
  };
}

// rtName is `t`'s type name, for a 42804 assignability message (the integer width is exact).
// typeNames renders a projection's resolved types as their canonical names for the public
// Outcome columnTypes — the `# types:` directive's assertion surface (spec/design/conformance.md
// §7). Same names as the 42804 message (rtName): the exact integer width, the unconstrained
// "decimal".
export function typeNames(ts: ResolvedType[]): string[] {
  return ts.map(rtName);
}

// rtCollatable reports whether a resolved type carries a collation (spec/design/collation.md §1):
// text or text[].
export function rtCollatable(t: ResolvedType): boolean {
  return t.kind === "text" || (t.kind === "array" && t.elem.kind === "text");
}

export function rtName(t: ResolvedType): string {
  switch (t.kind) {
    case "int":
      return canonicalName(t.ty);
    case "float":
      return canonicalName(t.ty);
    case "bool":
      return "boolean";
    case "text":
      return "text";
    case "decimal":
      return "decimal";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    case "interval":
      return "interval";
    case "json":
      return "json";
    case "jsonb":
      return "jsonb";
    case "jsonpath":
      return "jsonpath";
    case "composite":
      // A named composite is its type name; an anonymous ROW(...) is `record` (PG).
      return t.name ?? "record";
    case "array":
      return rtName(t.elem) + "[]";
    case "range": {
      // A range names itself by its element subtype (i32 → i32range — spec/design/ranges.md).
      const s = resolvedRangeElementScalar(t.elem);
      if (s !== undefined) {
        const name = rangeNameForElement(s);
        if (name !== undefined) return name;
      }
      return `range<${rtName(t.elem)}>`;
    }
    case "null":
      return "unknown";
  }
}

// resolvedRangeElementScalar returns the scalar element type of a resolved range element. A range's
// element is always one of the six scalar subtypes; undefined for anything else (never a valid
// range). Used to name a range and to build its codec.
export function resolvedRangeElementScalar(elem: ResolvedType): ScalarType | undefined {
  switch (elem.kind) {
    case "int":
      return elem.ty;
    case "decimal":
      return "decimal";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    default:
      return undefined;
  }
}

// === WITH RECURSIVE analysis (spec/design/recursive-cte.md) ==========================
//
// A WITH RECURSIVE CTE is recursive iff its body references its own name (anywhere, deep). A
// recursive CTE must take the well-formed shape `non_recursive_term UNION [ALL] recursive_term`
// with the self-reference appearing exactly once, as a direct FROM/JOIN relation of the recursive
// term. These structural checks mirror PostgreSQL's checkWellFormedRecursion, run on the parsed AST
// before planning; the error surface is recursive-cte.md §6. The `name` argument is the
// exact CTE name.

// analyzeRecursiveCte classifies a CTE body for WITH RECURSIVE (recursive-cte.md §6). It returns
// { recursive: false } when the body does not reference name (an ordinary CTE, even under
// RECURSIVE); otherwise it validates the recursive shape and returns { recursive: true, unionAll },
// or throws (42P19 for a malformed recursion, 0A000 for a deferred shape).
export function analyzeRecursiveCte(
  name: string,
  body: QueryExpr,
  searchCycle = false,
): { recursive: boolean; unionAll: boolean } {
  if (countSelfRefsQuery(body, name) === 0) {
    return { recursive: false, unionAll: false };
  }
  if (body.kind !== "setOp" || body.op !== "union") {
    throw engineError(
      "invalid_recursion",
      `recursive query "${name}" does not have the form non-recursive-term UNION [ALL] recursive-term`,
    );
  }
  if (body.orderBy.length > 0) {
    throw engineError("feature_not_supported", "ORDER BY in a recursive query is not implemented");
  }
  if (body.limit !== null || body.offset !== null) {
    throw engineError("feature_not_supported", "LIMIT in a recursive query is not implemented");
  }
  if (countSelfRefsQuery(body.lhs, name) > 0) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear within its non-recursive term`,
    );
  }
  // SEARCH / CYCLE need each side of the UNION to be one SELECT (PostgreSQL's messages and codes).
  if (searchCycle) {
    if (body.lhs.kind === "setOp") {
      throw engineError(
        "feature_not_supported",
        "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT",
      );
    }
    if (body.rhs.kind === "setOp") {
      throw engineError(
        "syntax_error",
        "with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT",
      );
    }
  }
  if (body.rhs.kind === "withExpr") {
    throw engineError(
      "feature_not_supported",
      "a nested WITH in the recursive term of a recursive query is not supported yet",
    );
  }
  if (body.rhs.kind !== "select") {
    throw engineError(
      "feature_not_supported",
      "a set operation in the recursive term of a recursive query is not supported yet",
    );
  }
  validateRecursiveTerm(name, body.rhs);
  return { recursive: true, unionAll: body.all };
}

// validateRecursiveTerm validates the recursive term (the UNION's right SELECT) of a recursive CTE
// (recursive-cte.md §6). The self-reference must appear exactly once, as a direct FROM/JOIN
// relation, not on the nullable side of an outer join; the term must contain no aggregate. The
// checks fire in PostgreSQL's order — a self-reference in a bad CONTEXT (a sublink, an outer join)
// is reported as that context even when a valid FROM reference also exists.
export function validateRecursiveTerm(name: string, sel: Select): void {
  if (countSublinkSelfRefs(sel, name) >= 1) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear within a subquery`,
    );
  }
  if (countFromSubquerySelfRefs(sel, name) >= 1) {
    throw engineError(
      "feature_not_supported",
      `recursive reference to query "${name}" inside a FROM subquery is not supported yet`,
    );
  }
  const direct = countDirectFromSelfRefs(sel, name);
  if (direct > 1) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear more than once`,
    );
  }
  if (itemsHaveAggregate(sel.items) || (sel.having !== null && exprHasAggregate(sel.having))) {
    throw engineError(
      "invalid_recursion",
      "aggregate functions are not allowed in a recursive query's recursive term",
    );
  }
  if (direct === 1 && directSelfRefOnNullableSide(sel, name)) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear within an outer join`,
    );
  }
}

// withHasDml reports whether a WITH statement contains any data-modifying part — a data-modifying CTE
// body or a data-modifying primary (spec/design/writable-cte.md). Such a statement runs through the
// writable-CTE orchestrator (the read pin + lexical-order, all-or-nothing execution); a pure-query
// WITH keeps the runWith path.
export function withHasDml(wq: WithQuery): boolean {
  return cteBodyIsDataModifying(wq.body) || wq.ctes.some((c) => cteBodyIsDataModifying(c.body));
}

// cteModes computes each CTE binding's evaluation mode (spec/design/cte.md §3, writable-cte.md §3): a
// RECURSIVE or data-modifying CTE is ALWAYS materialized; otherwise a MATERIALIZED hint or >=2
// references → materialize, else inline.
export function cteModes(bindings: CteBinding[]): CteMode[] {
  return bindings.map((b) => {
    if (b.recursive !== null || b.source.kind === "dml") return "materialize";
    // A body planned at its reference is inlined there (planner.md §3.4), even if the static count
    // missed a reference the planner then resolved.
    if (b.expanded) return "inline";
    if (b.hint === true) return "materialize";
    if (b.hint === false) return "inline";
    return b.refs >= 2 ? "materialize" : "inline";
  });
}

// staticCteRefs counts the static references to CTE `name` in one part of a WITH statement, as
// [query, data-modifying]: a data-modifying part's references are counted apart, because such a part
// is planned only when it runs, so a CTE it names is never inlined at its reference (planner.md
// §3.4). A query expression is a CteBody too, so it also serves the primary of a nested WITH.
function staticCteRefs(body: CteBody, name: string): [number, number] {
  const n = countCteRefsDml(body, name);
  return cteBodyIsDataModifying(body) ? [0, n] : [n, 0];
}

// cteReferenceCandidate reports whether the CTE at ci in a WITH list may be inlined at its reference
// (planner.md §3.4): not MATERIALIZED, and named exactly once by the later CTE bodies and `primary`
// (the query that owns the list), by a query part.
export function cteReferenceCandidate(ctes: Cte[], ci: number, primary: CteBody): boolean {
  const name = ctes[ci]!.name;
  let [query, dml] = staticCteRefs(primary, name);
  for (const later of ctes.slice(ci + 1)) {
    const [q, d] = staticCteRefs(later.body, name);
    query += q;
    dml += d;
  }
  return query === 1 && dml === 0 && ctes[ci]!.materialized !== true;
}

// addOutcomeCost adds extra cost to an outcome (the writable-CTE orchestrator folds the
// materialization cost of the data-modifying / query CTEs into the primary's result —
// spec/design/writable-cte.md §8).
export function addOutcomeCost(outcome: Outcome, extra: bigint): Outcome {
  return { ...outcome, cost: outcome.cost + extra };
}

// countCteRefsDml counts references to CTE `name` reachable through a cte_body's inner queries — the
// writable-CTE analogue of countSelfRefsQuery (spec/design/writable-cte.md §3). A query body delegates
// to the query counter; a data-modifying body counts the references in its source query / WHERE / SET
// RHSs / ON CONFLICT / RETURNING sublinks. Used by the orchestrator to count the references a
// NON-planned data-modifying part contributes to the inline-vs-materialize decision.
export function countCteRefsDml(body: CteBody, name: string): number {
  if (body.kind === "select" || body.kind === "setOp" || body.kind === "withExpr") {
    return countSelfRefsQuery(body, name);
  }
  if (body.kind === "insert") {
    let n = 0;
    if (body.source.kind === "query") {
      n = countSelfRefsQuery(body.source.query, name);
    } else {
      // A VALUES expression slot may hold a sublink (grammar.md §16).
      for (const values of body.source.rows) {
        for (const v of values) if (v.kind === "expr") n += countSelfRefsExpr(v.expr, name);
      }
    }
    if (body.onConflict?.doUpdate) {
      for (const a of body.onConflict.assignments) n += countSelfRefsExpr(a.value, name);
      if (body.onConflict.filter !== null) n += countSelfRefsExpr(body.onConflict.filter, name);
    }
    return n + countReturningRefs(body.returning, name);
  }
  if (body.kind === "update") {
    let n = 0;
    for (const a of body.assignments) n += countSelfRefsExpr(a.value, name);
    if (body.filter !== null) n += countSelfRefsExpr(body.filter, name);
    return n + countReturningRefs(body.returning, name);
  }
  // delete
  let n = 0;
  if (body.filter !== null) n += countSelfRefsExpr(body.filter, name);
  return n + countReturningRefs(body.returning, name);
}

// countReturningRefs counts references to CTE `name` in a RETURNING item list's sublinks.
export function countReturningRefs(returning: ReturningClause | null, name: string): number {
  if (returning === null || returning.items.kind !== "list") return 0;
  return returning.items.items.reduce((a, it) => a + countSelfRefsExpr(it.expr, name), 0);
}

// countSelfRefsQuery counts self-references to name anywhere in a query expression (deep — FROM
// relations at every nesting level plus expression sublinks).
export function countSelfRefsQuery(qe: QueryExpr, name: string): number {
  if (qe.kind === "select") return countSelfRefsSelect(qe, name);
  if (qe.kind === "withExpr") {
    // A nested WITH inherits this binding until an inner same-named CTE is declared. That inner
    // binding shadows it for later bodies and the main query, but its own non-recursive body still
    // sees the inherited binding (cte.md §7).
    let n = 0;
    let shadowed = false;
    for (const cte of qe.ctes) {
      if (!shadowed) n += countCteRefsDml(cte.body, name);
      if (cte.name === name) shadowed = true;
    }
    if (!shadowed) n += countSelfRefsQuery(qe.body, name);
    return n;
  }
  return countSelfRefsQuery(qe.lhs, name) + countSelfRefsQuery(qe.rhs, name);
}

// countSelfRefsSelect counts self-references in a SELECT: its FROM relations (deep) plus all of its
// expressions' sublinks.
export function countSelfRefsSelect(s: Select, name: string): number {
  let n = 0;
  for (const tref of fromRelations(s)) n += countSelfRefsTableref(tref, name);
  for (const e of selectExprs(s)) n += countSelfRefsExpr(e, name);
  return n;
}

// countSelfRefsTableref counts self-references reachable through one FROM relation: a plain table
// reference with the matching name (+1), a derived-table subquery (recurse), or a table-function's
// / VALUES' argument exprs.
export function countSelfRefsTableref(tref: TableRef, name: string): number {
  if (isPlainRelation(tref)) return tref.name === name ? 1 : 0;
  let n = 0;
  if (tref.subquery !== undefined) n += countSelfRefsQuery(tref.subquery, name);
  if (tref.args !== null && tref.args !== undefined) {
    for (const a of tref.args) n += countSelfRefsExpr(a, name);
  }
  if (tref.values !== undefined) {
    for (const row of tref.values) for (const e of row) n += countSelfRefsExpr(e, name);
  }
  return n;
}

// countSelfRefsExpr counts self-references inside an expression — only reachable through a sublink
// (a subquery is an independent query whose own FROM may reference the CTE). The walk is exhaustive
// (like exprHasAggregate).
export function countSelfRefsExpr(e: Expr, name: string): number {
  switch (e.kind) {
    case "scalarSubquery":
    case "exists":
      return countSelfRefsQuery(e.query, name);
    case "inSubquery":
    case "quantifiedSubquery":
      return countSelfRefsExpr(e.lhs, name) + countSelfRefsQuery(e.query, name);
    case "cast":
    case "collate":
      return countSelfRefsExpr(e.inner, name);
    case "extract":
      return countSelfRefsExpr(e.source, name);
    case "unary":
    case "isNull":
    case "isJson":
    case "jsonCtor":
      return countSelfRefsExpr(e.operand, name);
    case "jsonExists":
    case "jsonValue":
    case "jsonQuery":
      return (
        countSelfRefsExpr(e.ctx, name) +
        countSelfRefsExpr(e.path, name) +
        e.passing.reduce((n, p) => n + countSelfRefsExpr(p.expr, name), 0)
      );
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return countSelfRefsExpr(e.lhs, name) + countSelfRefsExpr(e.rhs, name);
    case "in":
      return (
        countSelfRefsExpr(e.lhs, name) + e.list.reduce((a, x) => a + countSelfRefsExpr(x, name), 0)
      );
    case "between":
      return (
        countSelfRefsExpr(e.lhs, name) +
        countSelfRefsExpr(e.lo, name) +
        countSelfRefsExpr(e.hi, name)
      );
    case "case":
      return (
        (e.operand !== null ? countSelfRefsExpr(e.operand, name) : 0) +
        e.whens.reduce(
          (a, w) => a + countSelfRefsExpr(w.cond, name) + countSelfRefsExpr(w.result, name),
          0,
        ) +
        (e.els !== null ? countSelfRefsExpr(e.els, name) : 0)
      );
    case "coalesce":
    case "greatestLeast":
    case "nullIf":
      return e.args.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "funcCall":
      return e.args.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "row":
      return e.fields.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "array":
      return e.elements.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "fieldAccess":
    case "fieldStar":
      return countSelfRefsExpr(e.base, name);
    case "qualifiedStar":
      return 0; // a leaf relation reference — no sublink to recurse into
    case "subscript":
      return (
        countSelfRefsExpr(e.base, name) +
        astSubscriptExprs(e.subscripts).reduce((a, x) => a + countSelfRefsExpr(x, name), 0)
      );
    case "quantified":
      return countSelfRefsExpr(e.lhs, name) + countSelfRefsExpr(e.array, name);
    default:
      return 0;
  }
}

// countDirectFromSelfRefs counts self-references that are DIRECT FROM/JOIN relations of this SELECT
// (a plain table ref matching the name). This is the only valid position for a recursive reference.
export function countDirectFromSelfRefs(s: Select, name: string): number {
  let n = 0;
  for (const tref of fromRelations(s)) {
    if (isPlainRelation(tref) && tref.name === name) n++;
  }
  return n;
}

// countFromSubquerySelfRefs counts self-references nested inside a FROM-position subquery /
// table-function args / VALUES of this SELECT (the deferred 0A000 shape).
export function countFromSubquerySelfRefs(s: Select, name: string): number {
  let n = 0;
  for (const tref of fromRelations(s)) {
    if (!isPlainRelation(tref)) n += countSelfRefsTableref(tref, name);
  }
  return n;
}

// countSublinkSelfRefs counts self-references reachable only through an expression sublink in this
// SELECT's top-level expressions — the `within a subquery` position.
export function countSublinkSelfRefs(s: Select, name: string): number {
  let n = 0;
  for (const e of selectExprs(s)) n += countSelfRefsExpr(e, name);
  return n;
}

// directSelfRefOnNullableSide reports whether the SELECT's single direct self-reference sits on the
// NULLABLE side of an outer join — the position PostgreSQL rejects. The FROM is a left-deep chain:
// relation 0 is `from`, relation i+1 is joins[i].table, combined by joins[i].kind. A LEFT/FULL join
// makes its right operand nullable; a RIGHT/FULL join makes the whole accumulated left nullable.
export function directSelfRefOnNullableSide(s: Select, name: string): boolean {
  const rels = fromRelations(s);
  const nullable = new Array<boolean>(rels.length).fill(false);
  for (let j = 0; j < s.joins.length; j++) {
    const right = j + 1;
    switch (s.joins[j]!.kind) {
      case "left":
        nullable[right] = true;
        break;
      case "right":
        for (let i = 0; i <= j; i++) nullable[i] = true;
        break;
      case "full":
        for (let i = 0; i <= right; i++) nullable[i] = true;
        break;
      default:
        break;
    }
  }
  return rels.some((tref, i) => isPlainRelation(tref) && tref.name === name && nullable[i]!);
}

// isPlainRelation reports whether a FROM relation is a plain table NAME — not a derived-table
// subquery, a table function, or a VALUES body. Only a plain relation can resolve to a CTE.
export function isPlainRelation(tref: TableRef): boolean {
  return (
    (tref.args === null || tref.args === undefined) &&
    tref.subquery === undefined &&
    tref.values === undefined
  );
}

// fromRelations returns the FROM relations of a SELECT in left-deep order: from (if present) then
// each join's table.
export function fromRelations(s: Select): TableRef[] {
  const rels: TableRef[] = [];
  if (s.from !== null) rels.push(s.from);
  for (const j of s.joins) rels.push(j.table);
  return rels;
}

// selectExprs returns every top-level expression of a SELECT that can hold a sublink (select items,
// WHERE, GROUP BY, HAVING, join ON conditions). ORDER BY keys are bare/qualified column references
// (never expressions), so they carry no sublink.
export function selectExprs(s: Select): Expr[] {
  const v: Expr[] = [];
  if (s.items.kind === "list") for (const it of s.items.items) v.push(it.expr);
  if (s.filter !== null) v.push(s.filter);
  for (const g of s.groupBy) forEachGroupExpr(g, (e) => v.push(e));
  if (s.having !== null) v.push(s.having);
  for (const j of s.joins) if (j.on !== null) v.push(j.on);
  return v;
}

// checkRecursiveColumnTypes checks a recursive CTE's column types (recursive-cte.md §2): the output
// types are FIXED by the non-recursive (anchor) term, and the recursive term's columns must be
// assignable to them — a literal adapts, an equal type passes, a WIDER type is 42804 (matching
// PostgreSQL). Mechanically the would-be UNION unified type must EQUAL the anchor type; any widening
// of the anchor is the error. An arity mismatch is 42601, like a plain UNION.
export function checkRecursiveColumnTypes(
  anchorAst: QueryExpr,
  anchor: QueryPlan,
  recursive: QueryPlan,
  name: string,
  unionAll: boolean,
  carried = 0,
): void {
  const a = anchor.columnTypes;
  const r = recursive.columnTypes.slice(0, recursive.columnTypes.length - carried);
  if (a.length !== r.length) {
    throw engineError("syntax_error", "each UNION query must have the same number of columns");
  }
  for (let i = 0; i < a.length; i++) {
    // PostgreSQL reports a column the recursive term changes at the non-recursive term's column.
    const atCol = outputColumnLocation(anchorAst, i);
    const unified = unifySetopColumn(a[i]!, r[i]!, "union");
    if (rtName(unified) !== rtName(a[i]!)) {
      throw engineError(
        "datatype_mismatch",
        `recursive query "${name}" column ${i + 1} has type ${rtName(a[i]!)} in non-recursive term but type ${rtName(unified)} overall`,
      )
        .hinted(RECURSIVE_TERM_TYPE)
        .withPosition(atCol);
    }
    // The non-recursive term also fixed the column's typmod, so the overall common typmod must
    // equal it (PostgreSQL compares type and typmod — create-table-as.md §2).
    const atMod = queryPlanColumnTypmods(anchor)[i]!;
    const overallMod = commonTypmod([atMod, queryPlanColumnTypmods(recursive)[i]!]);
    if (!typmodEqual(overallMod, atMod)) {
      throw engineError(
        "datatype_mismatch",
        `recursive query "${name}" column ${i + 1} has type ${typmodTypeName(a[i]!, atMod)} in non-recursive term but type ${typmodTypeName(a[i]!, overallMod)} overall`,
      )
        .hinted(RECURSIVE_TERM_TYPE)
        .withPosition(atCol);
    }
    if (!unionAll) requireSetopEquality(a[i]!);
    // The non-recursive term fixed the relation's collation, so the UNION's overall common
    // collation must equal it (PostgreSQL's recursive-CTE check, spec/design/collation.md §1): a
    // recursive term cannot re-collate a column. Mismatch is 42P21.
    if (rtCollatable(a[i]!)) {
      const ad = queryPlanColumnDerivs(anchor)[i]!;
      const overall = commonCollation([ad, queryPlanColumnDerivs(recursive)[i]!], unionAll);
      const nonRec = collationIdentity(ad);
      const all = collationIdentity(overall);
      if (nonRec !== all) {
        throw engineError(
          "collation_mismatch",
          `recursive query "${name}" column ${i + 1} has collation "${nonRec}" in non-recursive term but collation "${all}" overall`,
        )
          .hinted(RECURSIVE_TERM_COLLATION)
          .withPosition(atCol);
      }
    }
  }
}

// typmodTypeName is a resolved type's name with its typmod applied (`decimal(10,2)`, `varchar(5)`),
// as a column of it would be declared.
function typmodTypeName(ty: ResolvedType, typmod: Typmod): string {
  if (ty.kind === "decimal" && typmod.decimal !== null) {
    return `decimal(${typmod.decimal.precision},${typmod.decimal.scale})`;
  }
  if (ty.kind === "text" && typmod.varcharLen !== null) return `varchar(${typmod.varcharLen})`;
  return rtName(ty);
}

// collationIdentity is the collation a derivation settles to, named as PostgreSQL reports it (the
// recursive-CTE check, spec/design/collation.md §1): a collation's name, `default` for no collation
// (PG's default collation — a literal's), `(null)` for a conflicted / indeterminate one (PG's
// InvalidOid).
function collationIdentity(d: Deriv): string {
  if (d.kind === "implicit" || d.kind === "explicit") return d.name;
  if (d.kind === "none") return "default";
  return "(null)";
}

// cteSyntheticTable builds the synthetic relation a CTE reference resolves against
// (spec/design/cte.md §2): one column per body output, named by the rename list (a count mismatch
// with MORE aliases is 42P10) or the body's own output names, typed from the planned body. The
// relation has no primary key / constraints — it is read-only and its rows come from the CTE
// context, never a store. Each column carries its body output's collation derivation
// (spec/design/collation.md §1 "Derived relations").
export function cteSyntheticTable(name: string, plan: QueryPlan, rename: string[] | null): Table {
  return cteSyntheticTableCols(
    name,
    plan.columnNames,
    plan.columnTypes,
    queryPlanColumnDerivs(plan),
    queryPlanColumnTypmods(plan),
    rename,
  );
}

// cteSyntheticTableCols is the shared core of cteSyntheticTable, over explicit body column names +
// types + collation derivations + typmods (null = none for every column) — so a data-modifying CTE
// (whose "body output" is its RETURNING projection, not a QueryPlan) builds its synthetic relation
// the same way (spec/design/writable-cte.md §1). Each column records its body output's typmod, so a
// reference to it keeps the typmod (spec/design/create-table-as.md §2).
export function cteSyntheticTableCols(
  name: string,
  bodyNames: string[],
  bodyTypes: ResolvedType[],
  bodyDerivs: Deriv[] | null,
  bodyTypmods: Typmod[] | null,
  rename: string[] | null,
): Table {
  let colNames: string[];
  if (rename !== null) {
    // PostgreSQL allows FEWER aliases than the body has columns — the first `rename.length` columns
    // take the aliases, the rest keep their body output names (a partial rename). Only MORE aliases
    // than columns is an error (42P10).
    if (rename.length > bodyTypes.length) {
      throw engineError(
        "invalid_column_reference",
        `WITH query "${name}" has ${bodyTypes.length} columns available but ${rename.length} columns specified`,
      );
    }
    colNames = bodyTypes.map((_t, i) => rename[i] ?? bodyNames[i]!);
  } else {
    colNames = bodyNames.slice();
  }
  const columns: Column[] = colNames.map((n, i) => ({
    name: n,
    type: typeFromResolved(bodyTypes[i]!),
    decimal: null,
    varcharLen: null,
    temporal: null,
    primaryKey: false,
    notNull: false,
    default: null,
    defaultExpr: null,
    identity: null,
    collation: null,
  }));
  columns.forEach((col, i) => {
    setSyntheticCollation(col, bodyDerivs === null ? { kind: "none" } : bodyDerivs[i]!);
    const t = bodyTypmods === null ? TYPMOD_NONE : bodyTypmods[i]!;
    col.decimal = t.decimal;
    col.varcharLen = t.varcharLen;
    col.temporal = t.temporal;
  });
  return { name, columns, pk: [], checks: [], indexes: [], fks: [], exclusions: [] };
}

// setSyntheticCollation records a body output's collation derivation on its synthetic column
// (spec/design/collation.md §1 "Derived relations", PostgreSQL's rule): an implicit OR explicit body
// collation becomes the column's IMPLICIT collation (an outer reference is a plain column reference);
// any other body output has no collation and takes a neighbour's (PG's default). An indeterminate or
// conflicted body output has none either (PG's InvalidOid) — it still yields to a neighbour, but
// consuming it alone is 42P22.
export function setSyntheticCollation(col: Column, d: Deriv): void {
  if (d.kind === "implicit" || d.kind === "explicit") {
    if (d.name !== "C") col.collation = d.name;
  } else if (d.kind === "none") {
    col.noCollation = true;
  } else {
    col.collationConflict = true;
  }
}

// typeFromResolved is the catalog Type that round-trips a column's ResolvedType — used to give a
// CTE's synthetic columns a Type (spec/design/cte.md). An untyped NULL column maps to text
// (PostgreSQL's unknown -> text rule). A decimal's per-column typmod is irrelevant for a read-only
// CTE column (values flow through unchanged), so it is dropped. An anonymous ROW(...) composite
// becomes a record Type carrying its field list (spec/design/composite.md §1).
export function typeFromResolved(rt: ResolvedType): Type {
  switch (rt.kind) {
    case "int":
    case "float":
      return scalarT(rt.ty);
    case "bool":
      return scalarT("boolean");
    case "text":
    case "null":
      return scalarT("text");
    case "decimal":
      return scalarT("decimal");
    case "bytea":
      return scalarT("bytea");
    case "uuid":
      return scalarT("uuid");
    case "timestamp":
      return scalarT("timestamp");
    case "timestamptz":
      return scalarT("timestamptz");
    case "date":
      return scalarT("date");
    case "interval":
      return scalarT("interval");
    case "json":
      return scalarT("json");
    case "jsonb":
      return scalarT("jsonb");
    case "jsonpath":
      return scalarT("jsonpath");
    case "composite":
      if (rt.name !== null) return compositeT(rt.name);
      return recordT(rt.fields.map((f) => ({ name: f.name, ty: typeFromResolved(f.type) })));
    case "array":
      return arrayT(typeFromResolved(rt.elem));
    case "range":
      return rangeT(typeFromResolved(rt.elem));
  }
}

// ParamTypes accumulates the inferred type of each bind parameter ($N) across every clause of a
// statement (spec/design/api.md §5). types[i] is the inferred scalar type of $(i+1); a null entry
// marks a parameter referenced before any context fixed its type. Shared across every clause so a
// $1 used in both WHERE and the select list unifies, then finalized.
export class ParamTypes {
  // types[i] is $(i+1)'s inferred type: a scalar, or a container — an array, range, or named
  // composite (spec/design/api.md §5) — as the resolved column type the binder coerces into.
  types: (ColType | null)[] = [];
  // uncacheable is set during resolution when a node is created that makes the resolved plan
  // un-reusable across executions: a subquery (the uncorrelated-subquery fold rewrites it to a
  // constant baking in THIS execution's bound params) or a precompiled-regex node (whose one-shot
  // compileCharged cost flag mutates during eval, so a reused plan would under-charge the 2nd+
  // execute). A prepared statement's plan cache fills only when this stayed false — flagging at the
  // node's birth is complete regardless of where in the plan tree it lands (spec/design/api.md §2.4).
  uncacheable = false;
  // Dependencies accumulate recursively at operation resolution (index-dependencies.md).
  // Session timezone, clock, entropy, sequences, session variables, and runtime text-to-date
  // inputs make a persisted expression nonimmutable (42P17). This is separate from plan
  // cacheability: ordinary queries can evaluate these operations afresh on every execution.
  nonimmutable = false;
  indexContext = false;
  timezoneDeps: TimeZoneDeps = { dynamic: false, zones: [] };
  noteIndexZone(node: RExpr, env: Environment): void {
    if (this.indexContext) noteIndexZone(this.timezoneDeps, node, env);
  }

  // noteInputZone records a resolve-time literal fold that read an offset-less timestamptz in the
  // session zone (timezones.md §9.3): the folded instant depends on the zone, so the plan must not be
  // shared through a cache (a cached plan serves every session) and must not persist (an index
  // expression or predicate is re-resolved per statement, in whatever zone that statement runs
  // under — 42P17).
  noteInputZone(iz: InputZone): void {
    if (iz.consulted) {
      this.uncacheable = true;
      this.nonimmutable = true;
    }
  }

  // note records that $(idx0+1) appears with scalar context type ty (null = no context here). It
  // unifies with any prior inference (noteCol).
  note(idx0: number, ty: ScalarType | null): void {
    this.noteCol(idx0, ty === null ? null : { kind: "scalar", scalar: ty });
  }

  // noteCol records that $(idx0+1) appears with context type ty, a scalar or a container (null = no
  // context here). It unifies with any prior inference: equal types agree, two integer widths widen
  // to the wider, an incompatible pair is 42804.
  noteCol(idx0: number, ty: ColType | null): void {
    while (idx0 >= this.types.length) this.types.push(null);
    if (ty !== null) {
      const prev = this.types[idx0]!;
      this.types[idx0] = prev === null ? ty : unifyParamType(prev, ty, idx0);
    }
    noteParamType(idx0, this.types[idx0]!);
  }

  // finalize returns the ordered parameter types. A slot referenced but never typed — including a
  // gap in $1..$N — is 42P18 indeterminate_datatype.
  finalize(): ColType[] {
    const out: ColType[] = [];
    for (let i = 0; i < this.types.length; i++) {
      const t = this.types[i]!;
      if (t === null) {
        throw engineError(
          "indeterminate_datatype",
          `could not determine data type of parameter $${i + 1}`,
        );
      }
      out.push(t);
    }
    return out;
  }
}

// unifyParamType unifies two inferred types for the same parameter: equal agrees; two integer
// widths widen to the wider; any other mismatch is 42804 (spec/design/api.md §5).
export function unifyParamType(a: ColType, b: ColType, idx0: number): ColType {
  if (a.kind === "scalar" && b.kind === "scalar") {
    if (a.scalar === b.scalar) return a;
    if (isInteger(a.scalar) && isInteger(b.scalar)) return rank(a.scalar) >= rank(b.scalar) ? a : b;
  } else if (colTypeEqual(a, b)) {
    return a;
  }
  throw engineError("datatype_mismatch", `inconsistent types inferred for parameter $${idx0 + 1}`);
}

// colTypeEqual is structural equality of two resolved column types.
function colTypeEqual(a: ColType, b: ColType): boolean {
  switch (a.kind) {
    case "scalar":
      return b.kind === "scalar" && a.scalar === b.scalar;
    case "array":
    case "range":
      return b.kind === a.kind && colTypeEqual(a.elem, b.elem);
    case "composite":
      return (
        b.kind === "composite" &&
        a.name === b.name &&
        a.fields.length === b.fields.length &&
        a.fields.every((f, i) => {
          const g = b.fields[i]!;
          return (
            f.name === g.name &&
            f.notNull === g.notNull &&
            f.varcharLen === g.varcharLen &&
            f.temporal?.fields === g.temporal?.fields &&
            f.temporal?.precision === g.temporal?.precision &&
            f.typmod?.precision === g.typmod?.precision &&
            f.typmod?.scale === g.typmod?.scale &&
            colTypeEqual(f.type, g.type)
          );
        })
      );
  }
}

// bindParams coerces each supplied bind value to its inferred parameter type, two-phase /
// all-or-nothing like INSERT (spec/design/api.md §5): a count mismatch is 42601 and every value
// is validated up front (22003/42804/22P02/23502 via storeValue) before any row is touched.
export function paramLabels(n: number): string[] {
  return Array.from({ length: n }, (_, i) => `$${i + 1}`);
}

export function bindParams(
  supplied: Value[],
  types: ColType[],
  iz: InputZone,
  labels: readonly string[] = paramLabels(types.length),
): Value[] {
  if (supplied.length !== types.length) {
    throw engineError(
      "syntax_error",
      `bind parameter count mismatch: statement expects ${types.length}, got ${supplied.length}`,
    );
  }
  // A text value bound to a timestamptz parameter reads an offset-less string in the session zone,
  // as PostgreSQL's text-format parameter input does (timezones.md §9.3).
  return types.map((ty, i) =>
    ty.kind === "scalar"
      ? storeValue(supplied[i]!, ty.scalar, null, null, null, false, labels[i]!, iz)
      : bindContainerParam(supplied[i]!, ty, labels[i]!, iz),
  );
}

// bindContainerParam coerces a host value to an array, range, or composite parameter type
// (spec/design/api.md §5). A text value is read by the type's input function, as a string literal
// assigned to the type is; a range value is canonicalized as a range constructor's result is; every
// other value takes the storage coercion of a column of the type.
function bindContainerParam(v: Value, ty: ColType, label: string, iz: InputZone): Value {
  if (v.kind === "text") {
    if (ty.kind === "array") return coerceStringToArray(v.text, ty.elem, iz);
    if (ty.kind === "range") return coerceStringToRange(v.text, rangeParamDesc(ty), iz);
    if (ty.kind === "composite") return coerceRecordTextToValue(v.text, ty, iz);
  }
  if (v.kind === "range" && ty.kind === "range" && !v.empty) {
    const elem = colTypeScalar(ty.elem);
    const bound = (b: Value | null): Value | null =>
      b === null ? null : storeValue(b, elem, null, null, null, false, label, iz);
    return finalizeRange(
      rangeParamDesc(ty),
      bound(v.lower),
      bound(v.upper),
      v.lowerInc,
      v.upperInc,
    );
  }
  return coerceForStore(v, ty, null, null, null, false, label, iz);
}

// rangeParamDesc is the range descriptor of a range parameter type.
function rangeParamDesc(ty: { kind: "range"; elem: ColType }): RangeDesc {
  const desc = rangeForElement(colTypeScalar(ty.elem));
  if (desc === undefined) throw new Error("a range parameter's element has a range type");
  return desc;
}

// rejectParamsForDDL throws 42601 if bind parameters are supplied to a CREATE/DROP TABLE (which
// has no expressions to bind — spec/design/api.md §5).
export function rejectParamsForDDL(params: Value[]): void {
  if (params.length > 0) {
    throw engineError("syntax_error", "bind parameters are not allowed in a DDL statement");
  }
}

// buildSequenceDef resolves a parsed SeqOptions set into a validated SequenceDef
// (spec/design/sequences.md §1/§14), shared by CREATE SEQUENCE and an IDENTITY column's
// `( seq_options )` (§13). The AS type (or the serial/identity-supplied default) sets the default +
// validated bounds; then validates INCREMENT (≠ 0), CACHE (≥ 1), explicit MIN/MAX within the type
// range, MINVALUE ≤ MAXVALUE, and START in [min, max] (each 22023); a fresh sequence starts with
// lastValue = start, isCalled = false. ownedBy carries the IDENTITY / serial owner link (undefined
// for a plain CREATE SEQUENCE).
export function buildSequenceDef(
  name: string,
  options: SeqOptions,
  ownedBy: SeqOwner | undefined,
): SequenceDef {
  // The value type (§14): `AS <type>` → the named type (22023 if not an integer type), else bigint.
  let dtype: SeqDataType = "bigint";
  if (options.dataType !== null) {
    const dt = seqDataTypeFromName(options.dataType);
    if (dt === undefined) {
      throw engineError(
        "invalid_parameter_value",
        "sequence type must be smallint, integer, or bigint",
      );
    }
    dtype = dt;
  }
  const [typeMin, typeMax] = seqDataTypeRange(dtype);
  const increment = options.increment ?? 1n;
  if (increment === 0n) {
    throw engineError("invalid_parameter_value", "INCREMENT must not be zero");
  }
  const cache = options.cache ?? 1n;
  if (cache < 1n) {
    throw engineError("invalid_parameter_value", `CACHE (${cache}) must be greater than zero`);
  }
  const [defMin, defMax] = seqDataTypeDefaultBounds(dtype, increment);
  // An explicit MAXVALUE/MINVALUE outside the type range is 22023 — checked (MAX first, PG order)
  // BEFORE the MIN > MAX consistency check (§14.2).
  if (
    options.maxValue !== null &&
    options.maxValue.value !== null &&
    options.maxValue.value > typeMax
  ) {
    throw engineError(
      "invalid_parameter_value",
      `MAXVALUE (${options.maxValue.value}) is out of range for sequence data type ${seqDataTypePgName(dtype)}`,
    );
  }
  if (
    options.minValue !== null &&
    options.minValue.value !== null &&
    options.minValue.value < typeMin
  ) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${options.minValue.value}) is out of range for sequence data type ${seqDataTypePgName(dtype)}`,
    );
  }
  // `{ value: v }` MINVALUE v / `{ value: null }` NO MINVALUE / outer null unset → the type default.
  const minValue =
    options.minValue !== null && options.minValue.value !== null ? options.minValue.value : defMin;
  const maxValue =
    options.maxValue !== null && options.maxValue.value !== null ? options.maxValue.value : defMax;
  // PG requires MINVALUE strictly less than MAXVALUE (a one-value sequence is rejected); jed
  // previously allowed `==` — corrected here so CREATE and ALTER (sequences.md §15.2) agree with PG.
  if (minValue >= maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${minValue}) must be less than MAXVALUE (${maxValue})`,
    );
  }
  // START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
  const start = options.start ?? (increment < 0n ? maxValue : minValue);
  seqBoundCheckStart(start, minValue, maxValue);
  return {
    name,
    increment,
    minValue,
    maxValue,
    start,
    cache,
    cycle: options.cycle ?? false,
    lastValue: start,
    isCalled: false,
    ownedBy,
  };
}

// seqBoundCheckStart is PG's START-in-bounds cross-check (init_params): start ∈ [min, max], else
// 22023 with PG's wording. Shared by CREATE (buildSequenceDef) and ALTER (applySeqAlter).
export function seqBoundCheckStart(start: bigint, minValue: bigint, maxValue: bigint): void {
  if (start < minValue) {
    throw engineError(
      "invalid_parameter_value",
      `START value (${start}) cannot be less than MINVALUE (${minValue})`,
    );
  }
  if (start > maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `START value (${start}) cannot be greater than MAXVALUE (${maxValue})`,
    );
  }
}

// seqBoundCheckLast is PG's last_value (RESTART) cross-check (init_params): the post-edit last_value ∈
// [min, max], else 22023. PG uses the "RESTART value …" wording even with no RESTART written (§15.2).
export function seqBoundCheckLast(lastValue: bigint, minValue: bigint, maxValue: bigint): void {
  if (lastValue < minValue) {
    throw engineError(
      "invalid_parameter_value",
      `RESTART value (${lastValue}) cannot be less than MINVALUE (${minValue})`,
    );
  }
  if (lastValue > maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `RESTART value (${lastValue}) cannot be greater than MAXVALUE (${maxValue})`,
    );
  }
}

// applySeqAlter re-edits an existing SequenceDef per ALTER SEQUENCE s <options>
// (spec/design/sequences.md §15.2) — PG init_params with isInit=false. Only the WRITTEN options
// change; lastValue/isCalled are preserved unless restart is given. `dtype` is the sequence data
// type the bounds are checked against: bigint for ALTER SEQUENCE (the value type is not persisted,
// §14.4), the column type for an identity column's `ALTER TABLE … ALTER COLUMN … SET`
// (spec/design/alter.md §2.8). options.dataType must be null (the caller rejects AS as 0A000 first).
export function applySeqAlter(
  existing: SequenceDef,
  options: SeqOptions,
  restart: SeqRestart | null,
  dtype: SeqDataType,
): SequenceDef {
  const def = { ...existing };
  if (options.increment !== null) {
    if (options.increment === 0n) {
      throw engineError("invalid_parameter_value", "INCREMENT must not be zero");
    }
    def.increment = options.increment;
  }
  if (options.cache !== null) {
    if (options.cache < 1n) {
      throw engineError(
        "invalid_parameter_value",
        `CACHE (${options.cache}) must be greater than zero`,
      );
    }
    def.cache = options.cache;
  }
  // NO MINVALUE/NO MAXVALUE recompute the default for the (possibly new) INCREMENT sign against
  // `dtype`. An explicit bound must lie in `dtype`'s range (MAX checked first, PG order); an
  // unwritten bound is preserved (PG keeps it even when the sign flips).
  const [defMin, defMax] = seqDataTypeDefaultBounds(dtype, def.increment);
  const [typeMin, typeMax] = seqDataTypeRange(dtype);
  if (
    options.maxValue !== null &&
    options.maxValue.value !== null &&
    options.maxValue.value > typeMax
  ) {
    throw engineError(
      "invalid_parameter_value",
      `MAXVALUE (${options.maxValue.value}) is out of range for sequence data type ${seqDataTypePgName(dtype)}`,
    );
  }
  if (
    options.minValue !== null &&
    options.minValue.value !== null &&
    options.minValue.value < typeMin
  ) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${options.minValue.value}) is out of range for sequence data type ${seqDataTypePgName(dtype)}`,
    );
  }
  if (options.minValue !== null) {
    def.minValue = options.minValue.value === null ? defMin : options.minValue.value;
  }
  if (options.maxValue !== null) {
    def.maxValue = options.maxValue.value === null ? defMax : options.maxValue.value;
  }
  if (def.minValue >= def.maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${def.minValue}) must be less than MAXVALUE (${def.maxValue})`,
    );
  }
  if (options.start !== null) def.start = options.start;
  // Cross-check 1: START ∈ [min, max].
  seqBoundCheckStart(def.start, def.minValue, def.maxValue);
  // RESTART (applied last, before the last_value cross-check).
  if (restart !== null) {
    def.lastValue = restart.toStart ? def.start : restart.value;
    def.isCalled = false;
  }
  // Cross-check 2: the preserved/restarted last_value ∈ [min, max].
  seqBoundCheckLast(def.lastValue, def.minValue, def.maxValue);
  if (options.cycle !== null) def.cycle = options.cycle;
  return def;
}

// serialPseudoType maps a serial pseudo-type name to its underlying integer scalar
// (spec/design/sequences.md §12) — serial/serial4 → i32, bigserial/serial8 → i64,
// smallserial/serial2 → i16. undefined for any other name. Recognized only in a CREATE TABLE
// column-type position; the match is byte-exact (the parser passes the folded / quoted name).
export function serialPseudoType(name: string): ScalarType | undefined {
  switch (name) {
    case "serial":
    case "serial4":
      return "i32";
    case "bigserial":
    case "serial8":
      return "i64";
    case "smallserial":
    case "serial2":
      return "i16";
    default:
      return undefined;
  }
}

// stmtIsWrite reports whether a statement mutates the database (so autocommit must capture +
// durably persist it). Reads (SELECT, set operations) run with no transaction (transactions.md
// §4.1) — UNLESS the read-shaped statement calls a sequence-mutating function (nextval), which
// makes it a write (spec/design/sequences.md §4).
export function stmtIsWrite(stmt: Statement): boolean {
  // EXPLAIN is a read: plain EXPLAIN plans without executing (even of a DML inner — it never mutates).
  // Only EXPLAIN ANALYZE runs the inner statement, so it is a write iff the inner is
  // (spec/design/explain.md §3).
  if (stmt.kind === "explain") {
    return stmt.analyze && stmtIsWrite(stmt.inner);
  }
  return (
    stmt.kind === "analyze" ||
    stmt.kind === "createTable" ||
    stmt.kind === "dropTable" ||
    stmt.kind === "alterTable" ||
    stmt.kind === "createIndex" ||
    stmt.kind === "dropIndex" ||
    stmt.kind === "reindex" ||
    stmt.kind === "createType" ||
    stmt.kind === "dropType" ||
    stmt.kind === "createSequence" ||
    stmt.kind === "alterSequence" ||
    stmt.kind === "dropSequence" ||
    stmt.kind === "createView" ||
    stmt.kind === "dropView" ||
    stmt.kind === "alterView" ||
    stmt.kind === "insert" ||
    stmt.kind === "update" ||
    stmt.kind === "delete" ||
    // A WITH statement with any data-modifying part is a write (it stages INSERT/UPDATE/DELETE effects
    // — writable-cte.md): it must take the write gate, accumulate into working, and commit.
    (stmt.kind === "with" && withHasDml(stmt)) ||
    // A read-shaped statement that calls nextval/setval IS a write (sequences.md §4): it must take
    // the write gate, stage the advance, and commit (autocommit) — and is 25006 in a READ ONLY
    // transaction, exactly like any other write.
    stmtCallsSeqMutator(stmt)
  );
}

// stmtCallsSeqMutator reports whether stmt's expression trees contain a sequence-MUTATING function
// call (nextval; in S2, setval) anywhere — which makes an otherwise read-shaped statement a write
// (sequences.md §4). Only the read-shaped statements need checking: INSERT/UPDATE/DELETE/DDL are
// already writes (stmtIsWrite short-circuits before this), and an INSERT VALUES slot is literal-only
// (no function call). currval is a pure read and is NOT counted.
export function stmtCallsSeqMutator(stmt: Statement): boolean {
  switch (stmt.kind) {
    case "select":
      return selectCallsSeqMutator(stmt);
    case "setOp":
      return setOpCallsSeqMutator(stmt);
    case "with":
      return (
        stmt.ctes.some((c) => cteBodyCallsSeqMutator(c.body)) || cteBodyCallsSeqMutator(stmt.body)
      );
    default:
      return false;
  }
}

// cteBodyCallsSeqMutator reports whether a cte_body calls a sequence-mutating function. A query body
// delegates to the query walk; a data-modifying body already makes the WITH a write (via withHasDml),
// so this is not reached for it — it is treated as a write regardless (writable-cte.md).
export function cteBodyCallsSeqMutator(body: CteBody): boolean {
  const q = cteBodyAsQuery(body);
  return q !== null ? queryCallsSeqMutator(q) : true;
}

export function queryCallsSeqMutator(qe: QueryExpr): boolean {
  if (qe.kind === "setOp") return setOpCallsSeqMutator(qe);
  if (qe.kind === "withExpr") {
    // A nested WITH's CTE bodies and main body may call a sequence mutator (cte.md §7).
    for (const c of qe.ctes) if (cteBodyCallsSeqMutator(c.body)) return true;
    return queryCallsSeqMutator(qe.body);
  }
  return selectCallsSeqMutator(qe);
}

export function setOpCallsSeqMutator(so: SetOp): boolean {
  return (
    queryCallsSeqMutator(so.lhs) ||
    queryCallsSeqMutator(so.rhs) ||
    orderKeysCallSeqMutator(so.orderBy)
  );
}

// orderKeysCallSeqMutator reports whether an ORDER BY / WITHIN GROUP / in-aggregate ORDER BY key
// list calls a sequence-mutating function.
export function orderKeysCallSeqMutator(keys: readonly OrderKey[] | null | undefined): boolean {
  return (keys ?? []).some((k) => k.expr !== null && exprCallsSeqMutator(k.expr));
}

// windowDefCallsSeqMutator reports whether a window definition's PARTITION BY, ORDER BY, or frame
// offsets call a sequence-mutating function.
export function windowDefCallsSeqMutator(w: WindowDef): boolean {
  return (
    w.partition.some(exprCallsSeqMutator) ||
    w.order.some((k) => exprCallsSeqMutator(k.expr)) ||
    frameOffsets(w).some(exprCallsSeqMutator)
  );
}

// frameOffsets is the offset expressions of a window frame's `n PRECEDING` / `n FOLLOWING` bounds.
export function frameOffsets(w: WindowDef): Expr[] {
  const out: Expr[] = [];
  if (w.frame == null) return out;
  for (const b of [w.frame.start, w.frame.end]) {
    if (b.kind === "preceding" || b.kind === "following") out.push(b.offset);
  }
  return out;
}

export function selectCallsSeqMutator(s: Select): boolean {
  const itemCalls =
    s.items.kind === "list" && s.items.items.some((i) => exprCallsSeqMutator(i.expr));
  return (
    itemCalls ||
    (s.from !== null && tableRefCallsSeqMutator(s.from)) ||
    s.joins.some(
      (j) => tableRefCallsSeqMutator(j.table) || (j.on !== null && exprCallsSeqMutator(j.on)),
    ) ||
    (s.filter !== null && exprCallsSeqMutator(s.filter)) ||
    s.groupBy.some((g) => {
      let found = false;
      forEachGroupExpr(g, (e) => {
        if (exprCallsSeqMutator(e)) found = true;
      });
      return found;
    }) ||
    (s.having !== null && exprCallsSeqMutator(s.having)) ||
    s.windows.some(([, w]) => windowDefCallsSeqMutator(w)) ||
    orderKeysCallSeqMutator(s.orderBy)
  );
}

export function tableRefCallsSeqMutator(t: TableRef): boolean {
  return (
    t.args?.some(exprCallsSeqMutator) ||
    (t.subquery !== undefined && queryCallsSeqMutator(t.subquery)) ||
    (t.values?.some((row) => row.some(exprCallsSeqMutator)) ?? false) ||
    (t.jsonTable !== undefined &&
      (exprCallsSeqMutator(t.jsonTable.ctx) ||
        exprCallsSeqMutator(t.jsonTable.path) ||
        t.jsonTable.passing.some((p) => exprCallsSeqMutator(p.expr))))
  );
}

// exprCallsSeqMutator is exhaustive over Expr (every kind is matched): true iff the tree contains a
// sequence-mutating call (nextval or setval).
export function exprCallsSeqMutator(e: Expr): boolean {
  switch (e.kind) {
    case "funcCall": {
      const n = e.name;
      return (
        n === "nextval" ||
        n === "setval" ||
        e.args.some(exprCallsSeqMutator) ||
        (e.filter != null && exprCallsSeqMutator(e.filter)) ||
        (e.over != null && windowDefCallsSeqMutator(e.over)) ||
        orderKeysCallSeqMutator(e.withinGroup) ||
        orderKeysCallSeqMutator(e.aggOrder)
      );
    }
    case "column":
    case "qualifiedColumn":
    case "literal":
    case "typedLiteral":
    case "param":
      return false;
    case "row":
      return e.fields.some(exprCallsSeqMutator);
    case "array":
      return e.elements.some(exprCallsSeqMutator);
    case "fieldAccess":
    case "fieldStar":
      return exprCallsSeqMutator(e.base);
    case "qualifiedStar":
      return false; // a leaf relation reference — no sequence-mutating call
    case "subscript":
      return (
        exprCallsSeqMutator(e.base) ||
        e.subscripts.some((s) =>
          s.isSlice
            ? (s.lower !== null && exprCallsSeqMutator(s.lower)) ||
              (s.upper !== null && exprCallsSeqMutator(s.upper))
            : exprCallsSeqMutator(s.index),
        )
      );
    case "cast":
      return exprCallsSeqMutator(e.inner);
    case "extract":
      return exprCallsSeqMutator(e.source);
    case "collate":
      return exprCallsSeqMutator(e.inner);
    case "unary":
    case "isNull":
    case "isJson":
    case "jsonCtor":
      return exprCallsSeqMutator(e.operand);
    case "jsonExists":
    case "jsonValue":
    case "jsonQuery":
      return (
        exprCallsSeqMutator(e.ctx) ||
        exprCallsSeqMutator(e.path) ||
        e.passing.some((p) => exprCallsSeqMutator(p.expr))
      );
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.rhs);
    case "in":
      return exprCallsSeqMutator(e.lhs) || e.list.some(exprCallsSeqMutator);
    case "between":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.lo) || exprCallsSeqMutator(e.hi);
    case "case":
      return (
        (e.operand !== null && exprCallsSeqMutator(e.operand)) ||
        e.whens.some((w) => exprCallsSeqMutator(w.cond) || exprCallsSeqMutator(w.result)) ||
        (e.els !== null && exprCallsSeqMutator(e.els))
      );
    case "coalesce":
    case "greatestLeast":
    case "nullIf":
      return e.args.some(exprCallsSeqMutator);
    case "scalarSubquery":
    case "exists":
      return queryCallsSeqMutator(e.query);
    case "inSubquery":
    case "quantifiedSubquery":
      return exprCallsSeqMutator(e.lhs) || queryCallsSeqMutator(e.query);
    case "quantified":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.array);
  }
}

// PrivReq is the privilege requirements collected from one statement (spec/design/session.md §5.3):
// the per-table privileges (each (table, privilege) pair), the named functions (each needs EXECUTE),
// and whether the statement is DDL (gated by allowDdl). Collected by an exhaustive AST walk
// (mirroring exprCallsSeqMutator).
export type PrivReq = {
  tables: { name: string; priv: Privilege }[];
  // relations is every FROM relation reference that is not a CTE, in source-walk order — what a view
  // reference may resolve to (views.md §4). db is the qualifier (undefined = bare).
  relations: { db: string | undefined; name: string }[];
  functions: string[];
  isDdl: boolean;
  // isTempDdl is whether the DDL targets a SESSION-LOCAL temporary table (CREATE TEMP TABLE) — gated
  // by allowTempDdl instead of allowDdl (spec/design/temp-tables.md §5). Set only for a CREATE TEMP; a
  // DROP is classified by resolving the name.
  isTempDdl: boolean;
};

// collectStmtPrivs collects the privilege requirements of stmt (spec/design/session.md §5.3).
// Transaction control carries none (handled before dispatch); DDL just sets isDdl.
export function collectStmtPrivs(stmt: Statement, req: PrivReq): void {
  const locals = new Set<string>();
  switch (stmt.kind) {
    case "analyze":
      req.isDdl = true;
      req.tables.push({ name: stmt.name, priv: "select" });
      break;
    // REINDEX is maintenance like ANALYZE (session.md §5.3): allowDdl, plus SELECT on the target
    // table — an INDEX target's owning table is added by the executor's privilege check, which can
    // resolve the index. DATABASE / SYSTEM need only allowDdl (PostgreSQL's database-owner check).
    // The temp split (allowTempDdl) does not apply: it governs schema creation and removal, not
    // maintenance (spec/design/indexes.md §10).
    case "reindex":
      req.isDdl = true;
      if (stmt.reindexKind === "table" && stmt.name !== undefined) {
        req.tables.push({ name: stmt.name, priv: "select" });
      }
      break;
    case "createTable":
      req.isDdl = true;
      // A temp table's DDL is gated by the temp-scoped split of allowDdl (temp-tables.md §5):
      // allowTempDdl for a session-local temp table.
      req.isTempDdl = stmt.temp;
      // `CREATE TABLE … AS` reads what its query reads, as PostgreSQL checks it when the query runs;
      // `WITH NO DATA` never runs the query, so it needs nothing on what the query names
      // (spec/design/create-table-as.md §4).
      if (stmt.asQuery?.withData) {
        collectQueryPrivs(stmt.asQuery.query, req, locals);
      }
      break;
    case "dropTable":
    case "alterTable":
    case "createIndex":
    case "dropIndex":
    case "createType":
    case "dropType":
    case "createSequence":
    case "dropSequence":
    case "alterSequence":
    case "createView":
    case "dropView":
    case "alterView":
      req.isDdl = true;
      break;
    case "insert":
      collectInsertPrivs(stmt, req, locals);
      break;
    case "select":
      collectSelectPrivs(stmt, req, locals);
      break;
    case "setOp":
      collectSetOpPrivs(stmt, req, locals);
      break;
    case "with":
      collectWithPrivs(stmt, req, locals);
      break;
    case "update":
      collectUpdatePrivs(stmt, req, locals);
      break;
    case "delete":
      collectDeletePrivs(stmt, req, locals);
      break;
    case "explain":
      // EXPLAIN requires the inner statement's privileges (EXPLAIN INSERT needs INSERT, matching PG).
      // Plain EXPLAIN never executes, but authorization is checked on the inner regardless.
      collectStmtPrivs(stmt.inner, req);
      break;
    default:
      // Transaction control (begin/commit/rollback) carries no privilege requirement.
      break;
  }
}

export function collectInsertPrivs(ins: Insert, req: PrivReq, locals: Set<string>): void {
  // The write target needs INSERT. A VALUES expression slot needs what its sublinks read and the
  // functions it calls (grammar.md §16); an INSERT … SELECT source needs SELECT on its tables.
  req.tables.push({ name: ins.table, priv: "insert" });
  // PostgreSQL's ON CONFLICT and RETURNING rules (session.md §5.3): a conflict target reads the
  // arbiter columns (SELECT), DO UPDATE also updates (UPDATE), and a RETURNING that reads a column
  // reads the target (SELECT). A target-less DO NOTHING and a column-free RETURNING read nothing.
  if (ins.onConflict !== null) {
    if (ins.onConflict.target !== null) req.tables.push({ name: ins.table, priv: "select" });
    if (ins.onConflict.doUpdate) req.tables.push({ name: ins.table, priv: "update" });
  }
  if (returningReadsColumns(ins.returning)) req.tables.push({ name: ins.table, priv: "select" });
  if (ins.source.kind === "query") {
    collectQueryPrivs(ins.source.query, req, locals);
  } else {
    for (const values of ins.source.rows) {
      for (const v of values) if (v.kind === "expr") collectExprPrivs(v.expr, req, locals);
    }
  }
  if (ins.onConflict?.doUpdate) {
    for (const a of ins.onConflict.assignments) collectExprPrivs(a.value, req, locals);
    if (ins.onConflict.filter !== null) collectExprPrivs(ins.onConflict.filter, req, locals);
  }
  collectReturningPrivs(ins.returning, req, locals);
}

export function collectUpdatePrivs(upd: Update, req: PrivReq, locals: Set<string>): void {
  req.tables.push({ name: upd.table, priv: "update" });
  // SELECT on the target if it reads any column — a WHERE, a RETURNING, or a column/subquery-
  // referencing assignment RHS (a constant-only SET a = 1 with no WHERE/RETURNING reads nothing).
  const reads =
    upd.filter !== null ||
    upd.returning !== null ||
    upd.assignments.some((a) => exprReadsColumns(a.value));
  if (reads) req.tables.push({ name: upd.table, priv: "select" });
  for (const a of upd.assignments) collectExprPrivs(a.value, req, locals);
  if (upd.filter !== null) collectExprPrivs(upd.filter, req, locals);
  collectReturningPrivs(upd.returning, req, locals);
}

export function collectDeletePrivs(del: Delete, req: PrivReq, locals: Set<string>): void {
  req.tables.push({ name: del.table, priv: "delete" });
  // DELETE reads the target's columns through a WHERE or a RETURNING.
  if (del.filter !== null || del.returning !== null) {
    req.tables.push({ name: del.table, priv: "select" });
  }
  if (del.filter !== null) collectExprPrivs(del.filter, req, locals);
  collectReturningPrivs(del.returning, req, locals);
}

export function collectQueryPrivs(qe: QueryExpr, req: PrivReq, locals: Set<string>): void {
  if (qe.kind === "setOp") collectSetOpPrivs(qe, req, locals);
  else if (qe.kind === "withExpr") {
    // A nested WITH inherits the enclosing CTE names, then adds its own forward-visible names
    // (under RECURSIVE, each name is also visible inside its own body).
    const scope = new Set(locals);
    for (const cte of qe.ctes) {
      if (qe.recursive) scope.add(cte.name);
      collectCteBodyPrivs(cte.body, req, scope);
      scope.add(cte.name);
    }
    collectQueryPrivs(qe.body, req, scope);
  } else collectSelectPrivs(qe, req, locals);
}

export function collectSetOpPrivs(so: SetOp, req: PrivReq, locals: Set<string>): void {
  collectQueryPrivs(so.lhs, req, locals);
  collectQueryPrivs(so.rhs, req, locals);
  collectOrderKeyPrivs(so.orderBy, req, locals);
}

export function collectWithPrivs(wq: WithQuery, req: PrivReq, locals: Set<string>): void {
  // A CTE name shadows a base table inside the WITH (a FROM <cte> is not a catalog object), so it is
  // added to the local scope and never privilege-checked. Forward-only visibility: each CTE body sees
  // the CTE names declared before it, and under RECURSIVE also its own name (recursive-cte.md §3). A
  // data-modifying body / primary needs the write privilege on its target table (writable-cte.md).
  const scope = new Set(locals);
  for (const cte of wq.ctes) {
    if (wq.recursive) scope.add(cte.name);
    collectCteBodyPrivs(cte.body, req, scope);
    scope.add(cte.name);
  }
  collectCteBodyPrivs(wq.body, req, scope);
}

// collectCteBodyPrivs collects the privilege requirements of a cte_body — a query, or a
// data-modifying statement (spec/design/writable-cte.md) which needs the write privilege on its
// target.
export function collectCteBodyPrivs(body: CteBody, req: PrivReq, locals: Set<string>): void {
  if (body.kind === "insert") collectInsertPrivs(body, req, locals);
  else if (body.kind === "update") collectUpdatePrivs(body, req, locals);
  else if (body.kind === "delete") collectDeletePrivs(body, req, locals);
  else collectQueryPrivs(body, req, locals);
}

export function collectSelectPrivs(s: Select, req: PrivReq, locals: Set<string>): void {
  if (s.from !== null) collectTableRefPrivs(s.from, req, locals);
  for (const j of s.joins) {
    collectTableRefPrivs(j.table, req, locals);
    if (j.on !== null) collectExprPrivs(j.on, req, locals);
  }
  if (s.items.kind === "list") {
    for (const it of s.items.items) collectExprPrivs(it.expr, req, locals);
  }
  if (s.filter !== null) collectExprPrivs(s.filter, req, locals);
  for (const g of s.groupBy) forEachGroupExpr(g, (e) => collectExprPrivs(e, req, locals));
  if (s.having !== null) collectExprPrivs(s.having, req, locals);
  for (const [, w] of s.windows) collectWindowDefPrivs(w, req, locals);
  collectOrderKeyPrivs(s.orderBy, req, locals);
}

// collectOrderKeyPrivs walks an ORDER BY / WITHIN GROUP / in-aggregate ORDER BY key list (a bare
// column or ordinal key has no expression).
export function collectOrderKeyPrivs(
  keys: readonly OrderKey[] | null | undefined,
  req: PrivReq,
  locals: Set<string>,
): void {
  for (const k of keys ?? []) if (k.expr !== null) collectExprPrivs(k.expr, req, locals);
}

// collectWindowDefPrivs walks a window definition's PARTITION BY and ORDER BY expressions and its
// frame offsets (window.md).
export function collectWindowDefPrivs(w: WindowDef, req: PrivReq, locals: Set<string>): void {
  for (const e of w.partition) collectExprPrivs(e, req, locals);
  for (const k of w.order) collectExprPrivs(k.expr, req, locals);
  for (const e of frameOffsets(w)) collectExprPrivs(e, req, locals);
}

export function collectTableRefPrivs(t: TableRef, req: PrivReq, locals: Set<string>): void {
  if (t.args !== null) {
    // A set-returning function used as a row source — EXECUTE on the function; its args are exprs.
    req.functions.push(t.name);
    for (const a of t.args) collectExprPrivs(a, req, locals);
  } else if (t.subquery !== undefined) {
    collectQueryPrivs(t.subquery, req, locals);
  } else if (t.values !== undefined) {
    for (const row of t.values) for (const e of row) collectExprPrivs(e, req, locals);
  } else if (t.jsonTable !== undefined) {
    // JSON_TABLE is syntax, not a named function or a table: its name is only the row source's
    // label. The context item, path, and PASSING values are expressions (json-table.md).
    collectExprPrivs(t.jsonTable.ctx, req, locals);
    collectExprPrivs(t.jsonTable.path, req, locals);
    for (const p of t.jsonTable.passing) collectExprPrivs(p.expr, req, locals);
  } else if (t.db !== undefined || !locals.has(t.name)) {
    // A base-table reference (not a CTE / derived-table label) — needs SELECT. A qualified name
    // never resolves to a CTE (attached-databases.md §3), so a CTE cannot shadow it.
    req.tables.push({ name: t.name, priv: "select" });
    req.relations.push({ db: t.db, name: t.name });
  }
}

// returningReadsColumns reports whether a RETURNING list reads the target's columns: `*` does, and
// so does any item that references a column or holds a subquery.
export function returningReadsColumns(returning: ReturningClause | null): boolean {
  if (returning === null) return false;
  if (returning.items.kind !== "list") return true;
  return returning.items.items.some((it) => exprReadsColumns(it.expr));
}

export function collectReturningPrivs(
  returning: ReturningClause | null,
  req: PrivReq,
  locals: Set<string>,
): void {
  if (returning !== null && returning.items.kind === "list") {
    for (const it of returning.items.items) collectExprPrivs(it.expr, req, locals);
  }
}

// collectExprPrivs is exhaustive over Expr (mirroring exprCallsSeqMutator): collect every named
// function call (EXECUTE) and walk every subquery (its tables need SELECT).
export function collectExprPrivs(e: Expr, req: PrivReq, locals: Set<string>): void {
  switch (e.kind) {
    case "funcCall":
      req.functions.push(e.name);
      for (const a of e.args) collectExprPrivs(a, req, locals);
      // The positions outside the argument list run expressions too: the FILTER condition, the
      // WITHIN GROUP and in-aggregate ORDER BY keys (aggregates.md §11/§13/§22), and an inline OVER
      // definition (a named one is walked with its WINDOW clause).
      if (e.filter != null) collectExprPrivs(e.filter, req, locals);
      collectOrderKeyPrivs(e.withinGroup, req, locals);
      collectOrderKeyPrivs(e.aggOrder, req, locals);
      if (e.over != null) collectWindowDefPrivs(e.over, req, locals);
      break;
    case "column":
    case "qualifiedColumn":
    case "literal":
    case "typedLiteral":
    case "param":
      break;
    case "row":
      for (const f of e.fields) collectExprPrivs(f, req, locals);
      break;
    case "array":
      for (const el of e.elements) collectExprPrivs(el, req, locals);
      break;
    case "fieldAccess":
    case "fieldStar":
      collectExprPrivs(e.base, req, locals);
      break;
    case "qualifiedStar":
      // `t.*` names a relation already in FROM — its SELECT privilege is required by the FROM
      // clause itself, so the star adds no new function/table privilege here.
      break;
    case "subscript":
      collectExprPrivs(e.base, req, locals);
      for (const s of e.subscripts) {
        if (s.isSlice) {
          if (s.lower !== null) collectExprPrivs(s.lower, req, locals);
          if (s.upper !== null) collectExprPrivs(s.upper, req, locals);
        } else {
          collectExprPrivs(s.index, req, locals);
        }
      }
      break;
    case "cast":
    case "collate":
      collectExprPrivs(e.inner, req, locals);
      break;
    case "extract":
      collectExprPrivs(e.source, req, locals);
      break;
    case "unary":
    case "isNull":
    case "isJson":
    case "jsonCtor":
      collectExprPrivs(e.operand, req, locals);
      break;
    case "jsonExists":
    case "jsonValue":
    case "jsonQuery":
      collectExprPrivs(e.ctx, req, locals);
      collectExprPrivs(e.path, req, locals);
      for (const p of e.passing) collectExprPrivs(p.expr, req, locals);
      break;
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      collectExprPrivs(e.lhs, req, locals);
      collectExprPrivs(e.rhs, req, locals);
      break;
    case "in":
      collectExprPrivs(e.lhs, req, locals);
      for (const x of e.list) collectExprPrivs(x, req, locals);
      break;
    case "between":
      collectExprPrivs(e.lhs, req, locals);
      collectExprPrivs(e.lo, req, locals);
      collectExprPrivs(e.hi, req, locals);
      break;
    case "case":
      if (e.operand !== null) collectExprPrivs(e.operand, req, locals);
      for (const w of e.whens) {
        collectExprPrivs(w.cond, req, locals);
        collectExprPrivs(w.result, req, locals);
      }
      if (e.els !== null) collectExprPrivs(e.els, req, locals);
      break;
    case "coalesce":
    case "greatestLeast":
    case "nullIf":
      for (const a of e.args) collectExprPrivs(a, req, locals);
      break;
    case "scalarSubquery":
    case "exists":
      collectQueryPrivs(e.query, req, locals);
      break;
    case "inSubquery":
    case "quantifiedSubquery":
      collectExprPrivs(e.lhs, req, locals);
      collectQueryPrivs(e.query, req, locals);
      break;
    case "quantified":
      collectExprPrivs(e.lhs, req, locals);
      collectExprPrivs(e.array, req, locals);
      break;
  }
}

// exprReadsColumns reports whether e reads a stored column or a subquery's rows — the trigger for an
// UPDATE's SELECT requirement on its target (spec/design/session.md §5.3). A column reference or any
// subquery counts; a pure constant / parameter expression does not. Exhaustive over Expr.
export function exprReadsColumns(e: Expr): boolean {
  switch (e.kind) {
    case "column":
    case "qualifiedColumn":
      return true;
    case "scalarSubquery":
    case "exists":
    case "inSubquery":
    case "quantifiedSubquery":
      return true;
    case "literal":
    case "typedLiteral":
    case "param":
      return false;
    case "row":
      return e.fields.some(exprReadsColumns);
    case "array":
      return e.elements.some(exprReadsColumns);
    case "fieldAccess":
    case "fieldStar":
      return exprReadsColumns(e.base);
    case "qualifiedStar":
      return true; // `t.*` reads the relation's columns (e.g. `RETURNING t.*`)
    case "subscript":
      return (
        exprReadsColumns(e.base) ||
        e.subscripts.some((s) =>
          s.isSlice
            ? (s.lower !== null && exprReadsColumns(s.lower)) ||
              (s.upper !== null && exprReadsColumns(s.upper))
            : exprReadsColumns(s.index),
        )
      );
    case "cast":
    case "collate":
      return exprReadsColumns(e.inner);
    case "extract":
      return exprReadsColumns(e.source);
    case "unary":
    case "isNull":
    case "isJson":
    case "jsonCtor":
      return exprReadsColumns(e.operand);
    case "jsonExists":
    case "jsonValue":
    case "jsonQuery":
      return (
        exprReadsColumns(e.ctx) ||
        exprReadsColumns(e.path) ||
        e.passing.some((p) => exprReadsColumns(p.expr))
      );
    case "funcCall":
      return e.args.some(exprReadsColumns);
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return exprReadsColumns(e.lhs) || exprReadsColumns(e.rhs);
    case "in":
      return exprReadsColumns(e.lhs) || e.list.some(exprReadsColumns);
    case "between":
      return exprReadsColumns(e.lhs) || exprReadsColumns(e.lo) || exprReadsColumns(e.hi);
    case "case":
      return (
        (e.operand !== null && exprReadsColumns(e.operand)) ||
        e.whens.some((w) => exprReadsColumns(w.cond) || exprReadsColumns(w.result)) ||
        (e.els !== null && exprReadsColumns(e.els))
      );
    case "coalesce":
    case "greatestLeast":
    case "nullIf":
      return e.args.some(exprReadsColumns);
    case "quantified":
      return exprReadsColumns(e.lhs) || exprReadsColumns(e.array);
  }
}

// stmtKind is a short label for a statement kind, for the 25006 read-only-violation message (the
// message text is informational — never matched; spec/design/conformance.md §2).
export function stmtKind(stmt: Statement): string {
  switch (stmt.kind) {
    case "analyze":
      return "ANALYZE";
    case "createTable":
      return "CREATE TABLE";
    case "dropTable":
      return "DROP TABLE";
    case "alterTable":
      return "ALTER TABLE";
    case "createIndex":
      return "CREATE INDEX";
    case "dropIndex":
      return "DROP INDEX";
    case "reindex":
      return "REINDEX";
    case "createType":
      return "CREATE TYPE";
    case "dropType":
      return "DROP TYPE";
    case "createSequence":
      return "CREATE SEQUENCE";
    case "alterSequence":
      return "ALTER SEQUENCE";
    case "dropSequence":
      return "DROP SEQUENCE";
    case "createView":
      return "CREATE VIEW";
    case "dropView":
      return "DROP VIEW";
    case "alterView":
      return "ALTER VIEW";
    case "insert":
      return "INSERT";
    case "update":
      return "UPDATE";
    case "delete":
      return "DELETE";
    case "explain":
      return "EXPLAIN";
    default:
      return "statement";
  }
}

// --- EXPLAIN rendering (spec/design/explain.md) ------------------------------------------------
// EXPLAIN renders the planner's chosen plan as a deterministic, cross-core-identical result set: an
// ordinary query Outcome whose structural depth/node/detail columns are always present; COSTS (the
// default), ANALYZE, and LANE append their deterministic columns.
// Every cell is non-empty and free of leading/trailing whitespace (the harness renders the actual cell
// raw but TrimSpaces the expected line), so indentation is the depth integer, never whitespace (§2).
// The renderer is hand-written per core (§5 forbids codegenning it); the corpus + explain.md are the
// contract. The Engine methods above walk the plan; these module-level helpers spell each token.

// ExplainRow is one rendered plan row before it becomes a Value tuple in explainOutcome.
export type ExplainRow = {
  depth: number;
  frameDepth: number;
  node: string;
  detail: string;
  estRows: bigint;
  estCost: bigint;
  actualCost: bigint;
};

// ExplainRender accumulates the rendered plan rows. emit normalizes an empty detail to the "-"
// sentinel so no cell renders blank (spec/design/explain.md §2).
export class ExplainRender {
  rows: ExplainRow[] = [];
  verbose = false;
  frameDepth = 0;
  private next = 0;
  private estimates: { rows: bigint; cost: bigint }[];
  private actual: bigint[];
  constructor(estimates: { rows: bigint; cost: bigint }[] = [], actual: bigint[] = []) {
    this.estimates = estimates;
    this.actual = actual;
  }
  setActualCosts(actual: bigint[]): void {
    this.actual = actual;
    for (let i = 0; i < this.rows.length; i++) this.rows[i]!.actualCost = actual[i] ?? 0n;
  }
  emit(depth: number, node: string, detail: string): void {
    const estimate = this.estimates[this.next++] ?? { rows: 0n, cost: 0n };
    this.rows.push({
      depth,
      frameDepth: this.frameDepth,
      node,
      detail: detail === "" ? "-" : detail,
      estRows: estimate.rows,
      estCost: estimate.cost,
      actualCost: this.actual[this.next - 1] ?? 0n,
    });
  }
}

export function explainActualCosts(
  estimates: { rows: bigint; cost: bigint }[],
  total: bigint,
): bigint[] {
  // Actual attribution is execution-only: an unexecuted metadata subtree must not inherit its
  // estimate. The statement root is exact; descendant checkpoints overwrite these zeroes.
  const actual = estimates.map(() => 0n);
  if (actual.length > 0) actual[0] = total;
  return actual;
}

// insertDetail renders an INSERT's ON CONFLICT disposition (or "-" when there is none).
export function insertDetail(ins: Insert): string {
  if (ins.onConflict === null) return "-";
  return ins.onConflict.doUpdate ? "on conflict do update" : "on conflict do nothing";
}

// cteDetail renders a CTE binding's attributes: its materialization mode (inlined vs materialized —
// the planner's choice) and whether it is recursive.
export function cteDetail(b: CteBinding, mode: CteMode): string {
  const parts = [mode === "materialize" ? "materialized" : "inlined"];
  if (b.recursive !== null) parts.push("recursive");
  return parts.join("; ");
}

// aggDetail renders an Aggregate node's attributes: the grouping-key count, aggregate count, the
// grouping-set count when there is more than one set, and the HAVING conjunct count.
export function aggDetail(sp: SelectPlan, verbose: boolean): string {
  const parts = [`groups=${sp.groupKeys.length} aggs=${sp.aggSpecs.length}`];
  if (sp.groupSets.length > 1) parts.push(`sets=${sp.groupSets.length}`);
  if (sp.having !== null) {
    parts.push(
      verbose ? `having=${renderRExpr(sp.having)}` : `having:conjuncts=${conjunctCount(sp.having)}`,
    );
  }
  return parts.join("; ");
}

// joinDetail renders a Nested Loop node's attributes: the join kind and the ON predicate's conjunct
// count (a CROSS join has no ON). The JoinKind labels (inner/cross/left/right/full) are the spelling.
export function joinDetail(j: PlanJoin, verbose: boolean): string {
  if (j.on === null) return j.kind;
  return verbose
    ? `${j.kind}; on=${renderRExpr(j.on)}`
    : `${j.kind}; on:conjuncts=${conjunctCount(j.on)}`;
}

// setOpNodeName is the node label for a set-operation kind.
export function setOpNodeName(op: SetOpKind): string {
  return op === "union" ? "Union" : op === "intersect" ? "Intersect" : "Except";
}

// withNote appends an elided-ORDER-BY note to a node's detail (replacing a "-" sentinel).
export function withNote(detail: string, note: string): string {
  if (note === "") return detail;
  if (detail === "" || detail === "-") return "ordered: " + note;
  return detail + "; ordered: " + note;
}

// limitDetail renders a Limit node's `limit=N` / `offset=M` attributes (an absent side is omitted).
export function limitDetail(limit: bigint | null, offset: bigint | null): string {
  const parts: string[] = [];
  if (limit !== null) parts.push(`limit=${limit}`);
  if (offset !== null) parts.push(`offset=${offset}`);
  return parts.length === 0 ? "-" : parts.join(" ");
}

// countTrue counts the set entries in a touched-set mask (null ⇒ 0, the DML-scan case).
export function countTrue(mask: boolean[] | null): number {
  if (mask === null) return 0;
  let n = 0;
  for (const b of mask) if (b) n++;
  return n;
}

// renderBoundTerms renders a primary-key bound's terms as `col <op> <src>` conjuncts joined by
// " and " — e.g. `id = $1`, `id >= 5 and id < 10`.
export function renderBoundTerms(col: string, terms: BoundTerm[]): string {
  return terms.map((t) => `${col} ${boundOpText(t.op)} ${renderBoundSrc(t.src)}`).join(" and ");
}

// boundOpText is the symbol for a bound comparison operator.
export function boundOpText(op: BinaryOp): string {
  switch (op) {
    case "eq":
      return "=";
    case "ne":
      return "<>";
    case "lt":
      return "<";
    case "gt":
      return ">";
    case "le":
      return "<=";
    case "ge":
      return ">=";
    default:
      return "?";
  }
}

// renderBoundSrc renders a bound's const-source operand: a bind parameter as `$N` (1-based), a
// correlated outer-column reference as `outer`, or a literal via renderBoundLit.
export function renderBoundSrc(e: RExpr | null): string {
  if (e === null) return "?";
  switch (e.kind) {
    case "param":
      return "$" + (e.index + 1);
    case "outerColumn":
      return "outer";
    case "column":
      // An index-nested-loop bound source — a column of an earlier join relation resolved per outer
      // row (cost.md §3 "JOIN"). Rendered generically (the global column index is not a user-facing
      // name, like the correlated `outer` case above).
      return "join";
    default:
      return renderBoundLit(e);
  }
}

// renderBoundLit renders a constant bound operand as a single-line token. A float is its
// PostgreSQL text (float.md §9); other values use their canonical renderer, quoting textual forms
// so the token stays structurally unambiguous.
export function renderBoundLit(e: RExpr): string {
  switch (e.kind) {
    case "constInt":
      return e.value.toString();
    case "constBool":
      return e.value ? "true" : "false";
    case "constDecimal":
      return e.value.render();
    case "constText":
      return quoteExplainText(e.value);
    case "constFloat":
      return renderFloatPg(e.value, e.ty === "f32");
    default:
      return "<value>";
  }
}

function quoteExplainText(value: string): string {
  return (
    "'" +
    value
      .replaceAll("\\", "\\\\")
      .replaceAll("'", "''")
      .replaceAll("\n", "\\n")
      .replaceAll("\r", "\\r")
      .replaceAll("\t", "\\t") +
    "'"
  );
}

function explainOp(op: BinaryOp): string {
  switch (op) {
    case "add":
      return "+";
    case "sub":
      return "-";
    case "mul":
      return "*";
    case "div":
      return "/";
    case "mod":
      return "%";
    case "eq":
      return "=";
    case "ne":
      return "<>";
    case "lt":
      return "<";
    case "gt":
      return ">";
    case "le":
      return "<=";
    case "ge":
      return ">=";
    case "and":
      return "and";
    case "or":
      return "or";
    case "concat":
      return "||";
    case "contains":
      return "@>";
    case "containedBy":
      return "<@";
    case "overlaps":
      return "&&";
    case "strictlyLeft":
      return "<<";
    case "strictlyRight":
      return ">>";
    case "notExtendRight":
      return "&<";
    case "notExtendLeft":
      return "&>";
    case "adjacent":
      return "-|-";
    case "jsonGet":
      return "->";
    case "jsonGetText":
      return "->>";
    case "jsonGetPath":
      return "#>";
    case "jsonGetPathText":
      return "#>>";
    case "jsonHasKey":
      return "?";
    case "jsonHasAnyKey":
      return "?|";
    case "jsonHasAllKeys":
      return "?&";
    case "jsonDeletePath":
      return "#-";
    case "jsonPathExists":
      return "@?";
    case "jsonPathMatch":
      return "@@";
  }
}

function explainCall(name: string, args: RExpr[]): string {
  return `${name}(${args.map(renderRExpr).join(", ")})`;
}

// renderRExpr is EXPLAIN's canonical resolved-expression printer. Slots are structural (`#N`) so
// aliases and host map order cannot affect the spelling; every compound is fully parenthesized.
export function renderRExpr(e: RExpr): string {
  const binary = (lhs: RExpr, op: string, rhs: RExpr): string =>
    `(${renderRExpr(lhs)} ${op} ${renderRExpr(rhs)})`;
  switch (e.kind) {
    case "column":
      return `#${e.index}`;
    case "outerColumn":
      return `outer(${e.level},${e.index})`;
    case "param":
      return `$${e.index + 1}`;
    case "constInt":
      return e.value.toString();
    case "constFloat":
      return renderFloatPg(e.value, e.ty === "f32");
    case "constBool":
      return e.value ? "true" : "false";
    case "constText":
      return quoteExplainText(e.value);
    case "constDecimal":
      return e.value.render();
    case "constBytea":
      return quoteExplainText(render({ kind: "bytea", bytes: e.value }));
    case "constUuid":
      return quoteExplainText(render({ kind: "uuid", bytes: e.value }));
    case "constTimestamp":
      return quoteExplainText(render({ kind: "timestamp", micros: e.value }));
    case "constTimestamptz":
      return quoteExplainText(render({ kind: "timestamptz", micros: e.value }));
    case "constDate":
      return quoteExplainText(render({ kind: "date", days: e.value }));
    case "constInterval":
      return quoteExplainText(render({ kind: "interval", iv: e.value }));
    case "constJson":
      return quoteExplainText(e.value);
    case "constJsonb":
      return quoteExplainText(render({ kind: "jsonb", node: e.value }));
    case "constJsonPath":
      return quoteExplainText(e.value);
    case "constNull":
      return "NULL";
    case "constArray":
    case "constRange":
      return quoteExplainText(render(e.value));
    case "row":
      return explainCall("row", e.fields);
    case "array":
      return `array[${e.elements.map(renderRExpr).join(", ")}]`;
    case "field":
      return `${renderRExpr(e.base)}.field${e.index}`;
    case "subscript": {
      const subs = e.subscripts.map((s) =>
        s.isSlice
          ? `${s.lower === null ? "" : renderRExpr(s.lower)}:${s.upper === null ? "" : renderRExpr(s.upper)}`
          : renderRExpr(s.index),
      );
      return renderRExpr(e.base) + subs.map((s) => `[${s}]`).join("");
    }
    case "cast":
      return `cast(${renderRExpr(e.operand)} as ${canonicalName(e.target)})`;
    case "arrayCast":
      return `array_cast(${renderRExpr(e.operand)})`;
    case "neg":
      return `(-${renderRExpr(e.operand)})`;
    case "not":
      return `(not ${renderRExpr(e.operand)})`;
    case "arith":
    case "compare":
      return binary(e.lhs, explainOp(e.op), e.rhs);
    case "and":
      return binary(e.lhs, "and", e.rhs);
    case "or":
      return binary(e.lhs, "or", e.rhs);
    case "jsonGet":
      return binary(
        e.base,
        e.op === "arrow"
          ? "->"
          : e.op === "arrowText"
            ? "->>"
            : e.op === "hashArrow"
              ? "#>"
              : "#>>",
        e.arg,
      );
    case "jsonContains":
      return binary(e.a, "@>", e.b);
    case "jsonHasKey":
      return binary(
        e.base,
        e.hasKeyKind === "one" ? "?" : e.hasKeyKind === "any" ? "?|" : "?&",
        e.arg,
      );
    case "jsonConcat":
      return binary(e.a, "||", e.b);
    case "jsonDelete":
      return binary(e.base, e.deleteKind === "path" ? "#-" : "-", e.arg);
    case "isNull":
      return `(${renderRExpr(e.operand)} is ${e.negated ? "not " : ""}null)`;
    case "isJson": {
      const predicate = e.jsonKind === "value" ? "" : ` ${e.jsonKind}`;
      return `(${renderRExpr(e.operand)} is ${e.negated ? "not " : ""}json${predicate}${e.uniqueKeys ? " with unique keys" : ""})`;
    }
    case "jsonCtor":
      return `json(${renderRExpr(e.operand)}${e.uniqueKeys ? " with unique keys" : ""})`;
    case "distinct":
      return binary(e.lhs, e.negated ? "is not distinct from" : "is distinct from", e.rhs);
    case "like":
      return binary(e.lhs, `${e.negated ? "not " : ""}${e.insensitive ? "ilike" : "like"}`, e.rhs);
    case "regex":
      return binary(e.lhs, `${e.negated ? "!" : ""}~${e.insensitive ? "*" : ""}`, e.rhs);
    case "casing":
      return explainCall(e.upper ? "upper" : "lower", [e.arg]);
    case "atTimeZone":
      return binary(e.value, "at time zone", e.zone);
    case "dateTrunc":
      return explainCall(
        "date_trunc",
        e.zone === null ? [e.unit, e.value] : [e.unit, e.value, e.zone],
      );
    case "extract":
      return `extract(${e.field} from ${renderRExpr(e.value)})`;
    case "dateConvert":
      return `cast(${renderRExpr(e.inner)} as ${canonicalName(e.to)})`;
    case "dateClock":
      return `date_clock(${e.offsetDays})`;
    case "coalesce":
      return explainCall("coalesce", e.args);
    case "greatestLeast":
      return explainCall(e.greatest ? "greatest" : "least", e.args);
    case "nullIf":
      return explainCall("nullif", e.args);
    case "scalarFunc":
      // bytea / text || renders as its operator, like the jsonb concat (it has no function-call name).
      if (e.func === "bytea_concat" || e.func === "text_concat")
        return binary(e.args[0]!, "||", e.args[1]!);
      return explainCall(e.func, e.args);
    case "hostFunc":
      // The host function name is carried on the node so EXPLAIN renders it without the registry
      // (extensibility.md §5.1).
      return explainCall(e.name, e.args);
    case "arrayFunc":
      return explainCall(e.func, e.args);
    case "rangeFunc":
      return explainCall(e.func, e.args);
    case "regexFunc":
      return explainCall(e.func === "substring" ? "substring" : `regexp_${e.func}`, e.args);
    case "rangeCtor":
      return explainCall(rangeNameForElement(e.elem) ?? "range", e.args);
    case "rangeOp":
      return explainCall(`range_${camelToSnake(e.op)}`, e.args);
    case "rangeSetOp":
      return explainCall(`range_${camelToSnake(e.op)}`, e.args);
    case "variadic":
      return explainCall(e.func, e.args);
    case "jsonPathFn": {
      const name =
        e.pathFnKind === "exists"
          ? "jsonb_path_exists"
          : e.pathFnKind === "existsSilent"
            ? "jsonb_path_exists_silent"
            : e.pathFnKind === "queryFirst"
              ? "jsonb_path_query_first"
              : e.pathFnKind === "queryArray"
                ? "jsonb_path_query_array"
                : e.pathFnKind === "match"
                  ? "jsonb_path_match"
                  : "jsonb_path_match_silent";
      return explainCall(e.tz ? name + "_tz" : name, e.args);
    }
    case "jsonSqlFn":
      return renderExplainJsonSql(e);
    case "jsonObjectFromArrays":
      return explainCall(e.json ? "json_object" : "jsonb_object", e.args);
    case "jsonImage":
      return explainCall(e.func, e.args);
    case "jsonSetInsert":
      return explainCall(e.lax ? "jsonb_set_lax" : `jsonb_${e.mode}`, e.args);
    case "jsonBuild":
      return explainCall(`${e.json ? "json" : "jsonb"}_build_${e.buildKind}`, e.args);
    case "case": {
      const arms = e.arms
        .map((a) => `when ${renderRExpr(a.cond)} then ${renderRExpr(a.result)}`)
        .join(" ");
      return `(case ${arms} else ${renderRExpr(e.els)} end)`;
    }
    case "quantified":
      return `${renderRExpr(e.lhs)} ${explainOp(e.op)} ${e.all ? "all" : "any"}(${renderRExpr(e.array)})`;
    case "widthBucketArray":
      return `width_bucket(${renderRExpr(e.operand)}, ${renderRExpr(e.thresholds)})`;
    case "inValues":
      return `${renderRExpr(e.lhs)} ${e.negated ? "not " : ""}in (${e.list.map(renderExplainValue).join(", ")})`;
    case "subquery": {
      if (e.subKind === "scalar") return "scalar(<subquery>)";
      if (e.subKind === "exists") return `${e.negated ? "not " : ""}exists(<subquery>)`;
      if (e.subKind === "in")
        return `${renderRExpr(e.lhs!)} ${e.negated ? "not " : ""}in (<subquery>)`;
      return `${renderRExpr(e.lhs!)} ${explainOp(e.op!)} ${e.all ? "all" : "any"}(<subquery>)`;
    }
  }
}

function renderExplainJsonSql(e: Extract<RExpr, { kind: "jsonSqlFn" }>): string {
  let out = `json_${e.sqlKind}(${renderRExpr(e.args[0]!)}, ${renderRExpr(e.args[1]!)}`;
  for (let i = 0; i < e.passingNames.length; i++) {
    out += i === 0 ? " passing " : ", ";
    out += `${renderRExpr(e.args[2 + i]!)} as ${quoteIdentifier(e.passingNames[i]!)}`;
  }
  const onEmpty = explainJsonBehavior(e.onEmpty, e.args[e.onEmptyDefault]);
  const onError = explainJsonBehavior(e.onError, e.args[e.onErrorDefault]);
  if (e.sqlKind === "exists") {
    out += ` ${onError} on error`;
  } else if (e.sqlKind === "value") {
    out += ` returning ${explainJsonReturning(e.returning, e.decimal)}`;
    out += ` ${onEmpty} on empty ${onError} on error`;
  } else {
    out += ` returning ${explainJsonReturning(e.returning, e.decimal)}`;
    out += ` ${explainJsonWrapper(e.wrapper)}`;
    out += e.keepQuotes ? " keep quotes on scalar string" : " omit quotes on scalar string";
    out += ` ${onEmpty} on empty ${onError} on error`;
  }
  return out + ")";
}

function explainJsonReturning(returning: ScalarType, decimal: DecimalTypmod | null): string {
  if (returning === "decimal" && decimal !== null) {
    return `decimal(${decimal.precision},${decimal.scale})`;
  }
  return canonicalName(returning);
}

function explainJsonWrapper(wrapper: JsonWrapper): string {
  if (wrapper === "without") return "without array wrapper";
  return `with ${wrapper} array wrapper`;
}

// explainJsonBehavior renders an ON EMPTY / ON ERROR behavior; a DEFAULT renders its expression.
function explainJsonBehavior(behavior: JsonOnBehavior, defaultExpr: RExpr | undefined): string {
  if (behavior === "default") {
    if (defaultExpr === undefined) throw new Error("a DEFAULT behavior carries its expression");
    return `default ${renderRExpr(defaultExpr)}`;
  }
  if (behavior === "emptyArray") return "empty array";
  if (behavior === "emptyObject") return "empty object";
  return behavior;
}

function camelToSnake(value: string): string {
  return value.replace(/[A-Z]/g, (letter) => `_${letter.toLowerCase()}`);
}

function renderExplainValue(value: Value): string {
  switch (value.kind) {
    case "null":
      return "NULL";
    case "int":
      return value.int.toString();
    case "bool":
      return value.value ? "true" : "false";
    case "decimal":
      return value.dec.render();
    case "f32":
    case "f64":
      return renderFloatPg(value.value, value.kind === "f32");
    default:
      return quoteExplainText(render(value));
  }
}

// conjunctCount retains the compact non-VERBOSE spelling. VERBOSE uses the complete resolved-
// expression printer (spec/design/explain.md §5).
export function conjunctCount(e: RExpr | null): number {
  if (e === null) return 0;
  if (e.kind === "and") return conjunctCount(e.lhs) + conjunctCount(e.rhs);
  return 1;
}

// cloneStores captures the committed stores cheaply for rollback-on-error: each store is an O(1)
// persistent-map clone (the catalog map of Table objects is shallow-copied by the caller, since
// Table objects are never mutated in place — only added/removed).
export function cloneStores(stores: Map<string, TableStore>): Map<string, TableStore> {
  const out = new Map<string, TableStore>();
  for (const [k, s] of stores) out.set(k, s.clone());
  return out;
}

// dmlOutcome wraps a DML statement's completion: a query result projecting the returned rows
// when a RETURNING clause was resolved (retNames non-null — grammar.md §32; zero affected
// rows is an EMPTY query result, never a bare statement), else a bare statement result
// carrying the affected-row count (spec/design/api.md §4).
export function dmlOutcome(
  retNames: string[] | null,
  retTypes: string[] | null,
  returned: Value[][] | null,
  affected: number,
  cost: bigint,
): Outcome {
  if (retNames !== null) {
    return {
      kind: "query",
      columnNames: retNames,
      columnTypes: retTypes ?? [],
      rows: returned ?? [],
      cost,
    };
  }
  return { kind: "statement", cost, rowsAffected: affected };
}
