// Writing through an automatically updatable view (spec/design/views.md §15): the layer chain a
// write builds and the pure helpers over it. The Engine methods that validate a write and retarget a
// statement (prepareViewWrite and the rewrite*Target trio) live in executor.ts. Mirrors
// impl/rust/src/executor/view_write.rs and impl/go view_write.go.
//
// A write through view `v` is the equivalent statement on the table at the bottom of v's view
// chain. prepareViewWrite validates the target (§15.3) and builds one WriteLayer per level: the
// columns visible at that level and what each one is — a reference to a column of the level below,
// or a read-only select item. The DML executors then run against the base table with a scope whose
// target relation carries the top layer, so a name binds to the view's column (a layered
// resolution). An updatable column resolves to the base column itself; a read-only one to its
// select item, resolved in the view body's isolated scope at the depth of the reference. The views'
// WHERE clauses resolve the same way and are AND-ed ahead of the statement's own; the checked views'
// WHERE clauses become the statement's check-option checks (resolveViewChecks, §15.6).

import type { CheckOption, ConflictColumn, ConflictTarget, Expr } from "./ast.ts";
import type { Column, Table } from "./catalog.ts";
import type { EngineError } from "./errors.ts";
import { engineError } from "./errors.ts";
import type { RExpr } from "./executor.ts";

// WriteKind is the statement kind a write through a view performs.
export type WriteKind = "insert" | "update" | "delete";

// writeVerb is the verb of the 55000 message (cannot <verb> view "v").
export function writeVerb(kind: WriteKind): string {
  return kind === "insert" ? "insert into" : kind === "update" ? "update" : "delete from";
}

// LayerCol is what one visible column of a WriteLayer is: a reference to column `ref` of the level
// below (the table's column when the layer is the table), or a read-only select item written
// against the level below under belowLabel.
export type LayerCol = { kind: "ref"; ref: number } | { kind: "expr"; expr: Expr };

// WriteLayer is one level of a write through a view chain (views.md §15.2). The bottom layer is the
// table: its columns are the table's (renamed by the referencing body's column alias list), each a
// reference to itself. A view layer's columns are the view's output columns.
export type WriteLayer = {
  // The view this layer is; null for the table.
  view: string | null;
  // The visible columns: names as visible at this level, with types and collations.
  columns: Column[];
  cols: LayerCol[];
  // The level the view's select items and WHERE are written against; null for the table.
  below: WriteLayer | null;
  // The label of the view body's FROM item (its alias, else the relation's name).
  belowLabel: string;
  // The view's WHERE, written against below.
  filter: Expr | null;
  // The view's own check option (views.md §15.6); null for the table.
  check: CheckOption | null;
};

// LayerFollow is where a layer column leads: a base table column, or the read-only select item that
// defines it (with the layer that owns it).
export type LayerFollow =
  | { kind: "base"; index: number }
  | { kind: "expr"; owner: WriteLayer; expr: Expr };

export function followLayer(layer: WriteLayer, col: number): LayerFollow {
  const c = layer.cols[col]!;
  if (c.kind === "expr") return { kind: "expr", owner: layer, expr: c.expr };
  return layer.below === null ? { kind: "base", index: c.ref } : followLayer(layer.below, c.ref);
}

// viewsBottomUp returns the view layers from the one nearest the table up to `layer`.
export function viewsBottomUp(layer: WriteLayer): WriteLayer[] {
  const out: WriteLayer[] = [];
  for (let l: WriteLayer | null = layer; l !== null; l = l.below) {
    if (l.view !== null) out.unshift(l);
  }
  return out;
}

// ViewWrite is a validated write through a view: the target view, the base table, the top layer,
// and the base column each statement target maps to (in the statement's target order). conflictSet
// holds the same for an INSERT … ON CONFLICT DO UPDATE's SET targets (views.md §15.7).
export type ViewWrite = {
  view: string;
  table: string;
  top: WriteLayer;
  targets: string[];
  conflictSet: string[];
};

// viewConflictTarget maps an ON CONFLICT target through the view to the base table (views.md
// §15.7): a column list names view columns, each of which must exist (42703) and lead to a base
// column — a read-only column leads to an expression, which no unique index's column list matches
// (42P10). A view has no constraints, so ON CONSTRAINT is always 42704.
export function viewConflictTarget(
  vw: ViewWrite,
  table: Table,
  target: ConflictTarget | null,
  checkNames: (c: ConflictColumn) => void,
): ConflictTarget | null {
  if (target === null) return null;
  if (target.kind === "constraint") {
    throw engineError(
      "undefined_object",
      `constraint ${target.name} for table ${vw.view} does not exist`,
    );
  }
  // Each element in order: its written ordering, the view column, then its decoration names
  // (upsert.md §2) — PostgreSQL resolves the target against the view before the rewrite maps it.
  const positions = target.columns.map((c) => {
    checkConflictOrdering(c);
    const k = vw.top.columns.findIndex((col) => col.name === c.name);
    if (k < 0) throw engineError("undefined_column", "column does not exist: " + c.name);
    checkNames(c);
    return k;
  });
  const columns = positions.map((k, i) => {
    const f = followLayer(vw.top, k);
    if (f.kind === "expr") {
      throw engineError(
        "invalid_column_reference",
        "there is no unique or exclusion constraint matching the ON CONFLICT specification",
      );
    }
    return { ...target.columns[i]!, name: table.columns[f.index]!.name };
  });
  return { kind: "columns", columns };
}

// checkConflictOrdering is the written-ordering check on one ON CONFLICT inference element
// (spec/design/upsert.md §2), raised before the element's column is looked up — PostgreSQL's
// order: ASC/DESC, then NULLS FIRST|LAST, each 42P10.
export function checkConflictOrdering(c: ConflictColumn): void {
  if (c.ordering) {
    throw engineError("invalid_column_reference", "ASC/DESC is not allowed in ON CONFLICT clause");
  }
  if (c.nulls) {
    throw engineError(
      "invalid_column_reference",
      "NULLS FIRST/LAST is not allowed in ON CONFLICT clause",
    );
  }
}

// ViewCheck is one check-option check of a write through a view (views.md §15.6): the checked view's
// name and its WHERE, resolved over the base row.
export type ViewCheck = { view: string; check: RExpr };

// checkOptionViolation is the 44000 a new row that fails a checked view's WHERE raises (views.md
// §15.6).
export function checkOptionViolation(view: string): EngineError {
  return engineError(
    "with_check_option_violation",
    `new row violates check option for view "${view}"`,
  );
}

// columnRefPosition is the position of the FROM item's column a select item names, when the item is
// a plain column reference (bare, or qualified by the FROM item's label) — an updatable view column
// (§15.1) — else -1.
export function columnRefPosition(e: Expr, label: string, below: string[]): number {
  if (e.kind === "column") return below.indexOf(e.name);
  if (e.kind === "qualifiedColumn" && e.qualifier === label) return below.indexOf(e.name);
  return -1;
}

// renamedNames applies a column alias list over a relation's column names (a partial rename,
// grammar.md §15).
export function renamedNames(names: string[], list: string[] | undefined): string[] {
  const out = names.slice();
  if (list !== undefined) {
    for (let i = 0; i < list.length && i < out.length; i++) out[i] = list[i]!;
  }
  return out;
}
