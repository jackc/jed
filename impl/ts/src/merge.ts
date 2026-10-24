// `MERGE` (spec/design/merge.md): the resolved plan shapes, the action list, and the INSERT-action
// column mapping. The Engine methods that resolve and run the statement (planMerge,
// executeMergeSteps) live in executor.ts, beside UPDATE and DELETE, whose helpers they share; this
// file holds the parts that need no Engine. Mirrors impl/rust/src/executor/merge.rs.

import type { Expr, MergeMatch, Overriding, SrcLoc } from "./ast.ts";
import type { Table } from "./catalog.ts";
import { columnIndex } from "./catalog.ts";
import { engineError, generatedAlwaysInsert } from "./errors.ts";
import type { QueryPlan, RExpr, ScopeRel } from "./executor.ts";
import type { Deriv, Typmod } from "./resolve.ts";
import type { ResolvedType } from "./executor.ts";
import type { Row } from "./storage.ts";
import type { AssignPlan } from "./window.ts";

// MergeActionPlan is a resolved WHEN clause's action. An INSERT's provided[i] is the value slot
// table column i takes (-1 — its default); values[p] is null for a slot that supplies no value (a
// DEFAULT slot, or a value an OVERRIDING USER VALUE identity column discards).
export type MergeActionPlan =
  | { kind: "update"; assigns: AssignPlan[] }
  | { kind: "delete" }
  | { kind: "insert"; provided: number[]; values: (RExpr | null)[] }
  | { kind: "doNothing" };

// MergeClausePlan is a resolved WHEN clause. Its condition and expressions read the clause kind's
// evaluation row: [target | source] for MATCHED, [source] for NOT MATCHED [BY TARGET], [target] for
// NOT MATCHED BY SOURCE (merge.md §2).
export type MergeClausePlan = {
  match: MergeMatch;
  condition: RExpr | null;
  action: MergeActionPlan;
};

// MergeReturning is a resolved RETURNING projection: the nodes and the output schema.
export type MergeReturning = {
  nodes: RExpr[];
  names: string[];
  types: ResolvedType[];
  derivs: Deriv[];
  typmods: Typmod[];
};

// MergePlan is a resolved MERGE (merge.md §2/§3), owning everything execution needs.
export type MergePlan = {
  source: QueryPlan;
  // The source's width.
  width: number;
  // The join condition over [target | source]: it decides a match.
  on: RExpr;
  // The join condition with the source columns as outer references (level 1): the target probe's
  // access path is planned from it (§3). It never decides a match.
  probe: RExpr;
  clauses: MergeClausePlan[];
  // The expression defaults the INSERT actions may apply, per column (constraints.md §2).
  defaultExprs: (RExpr | null)[];
  returning: MergeReturning | null;
};

// MergePlanned is one action of the action list (merge.md §4), in matching-pass order. src is the
// source row index (-1 for a not-matched-by-source row).
export type MergePlanned =
  | { kind: "insert"; row: Row; src: number }
  | {
      kind: "update";
      oldKey: Uint8Array;
      newKey: Uint8Array;
      newRow: Row;
      oldRow: Row;
      src: number;
      clause: number;
    }
  | { kind: "delete"; key: Uint8Array; oldRow: Row; src: number };

// mergeSourceRel is the source relation at flat offset, labeled by its alias or name (empty for an
// unaliased derived table, whose columns are then reachable only bare).
export function mergeSourceRel(table: Table, label: string, offset: number): ScopeRel {
  return { label, table, offset };
}

// mergeVersionRel is a qualifier-only row-version relation (old / new) over the target's columns.
export function mergeVersionRel(table: Table, label: string, offset: number): ScopeRel {
  return { label, table, offset, qualifierOnly: true };
}

// mergeTargetRel is the target relation at flat offset, labeled by its alias, else its name.
export function mergeTargetRel(table: Table, alias: string | undefined, offset: number): ScopeRel {
  return { label: alias ?? table.name, table, offset, base: true };
}

// mergeInsertColumns maps an INSERT action's value slots to target columns (merge.md §2):
// provided[i] is the slot table column i takes (-1 — its default: an omitted column, a DEFAULT slot,
// or an identity column under OVERRIDING USER VALUE), and slotCol[p] the column slot p fills. Without
// a column list the slots fill the first columns in order. Errors: an unknown column 42703, a repeated
// one 42701, more values than columns (or fewer than a list names) 42601, an explicit value for a
// GENERATED ALWAYS identity column without OVERRIDING SYSTEM VALUE 428C9.
export function mergeInsertColumns(
  table: Table,
  columns: string[] | null,
  columnLocs: SrcLoc[],
  overriding: Overriding | null,
  values: (Expr | null)[],
): { provided: number[]; slotCol: number[] } {
  const n = table.columns.length;
  const targets: number[] = [];
  if (columns !== null) {
    for (let p = 0; p < columns.length; p++) {
      const name = columns[p]!;
      const idx = columnIndex(table, name);
      if (idx < 0) {
        throw engineError(
          "undefined_column",
          `column ${name} of relation ${table.name} does not exist`,
        ).withPosition(columnLocs[p]);
      }
      if (targets.includes(idx)) {
        throw engineError(
          "duplicate_column",
          `column ${table.columns[idx]!.name} specified more than once`,
        );
      }
      targets.push(idx);
    }
  } else {
    for (let i = 0; i < n; i++) targets.push(i);
  }
  if (values.length > targets.length) {
    throw engineError("syntax_error", "INSERT has more expressions than target columns");
  }
  if (columns !== null && values.length < targets.length) {
    throw engineError("syntax_error", "INSERT has more target columns than expressions");
  }
  const provided: number[] = new Array(n).fill(-1);
  const slotCol: number[] = new Array(values.length).fill(-1);
  for (let p = 0; p < values.length; p++) {
    const i = targets[p]!;
    slotCol[p] = i;
    if (values[p] === null) continue;
    const col = table.columns[i]!;
    if (col.identity !== null && overriding === "user") continue;
    if (col.identity === "always" && overriding !== "system") {
      throw generatedAlwaysInsert(col.name);
    }
    provided[i] = p;
  }
  return { provided, slotCol };
}
