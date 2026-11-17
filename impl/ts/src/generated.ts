// Stored generated columns (spec/design/generated-columns.md): the statement-independent pieces —
// the kind gate, the structural pre-walk of a generation expression (§2 step 1), the recompute set
// an UPDATE derives from its SET targets (§3.2), the 428C9 / FK-action errors, and the dependency
// lookups ALTER TABLE needs (§4.4). Admission, resolution and per-row computation live on the
// executor (they need its scopes and evaluator).

import type { Expr, GeneratedDef } from "./ast.ts";
import type { Column, DefaultExpr, FkAction, Table } from "./catalog.ts";
import { type EngineError, engineError, generatedAlwaysInsert } from "./errors.ts";
import { astSubscriptExprs, checkReferencedColumns, isAggregateName } from "./executor.ts";

// requireStored is the 0A000 for a generation clause that is not STORED (§1).
export function requireStored(def: GeneratedDef): void {
  if (def.kind !== "stored") {
    throw engineError("feature_not_supported", "virtual generated columns are not supported");
  }
}

// generatedExprOf is the catalog form of a parsed generation clause (its expression and persisted
// text).
export function generatedExprOf(def: GeneratedDef): DefaultExpr {
  return { exprText: def.text, expr: def.expr };
}

// rejectGeneratedStructure applies the structural rejections for a generation expression (§2 step
// 1), one depth-first pre-walk before resolution, as for a CHECK: a subquery or set-returning
// function is 0A000, an aggregate 42803, a window call 42P20, a bind parameter 42P02.
export function rejectGeneratedStructure(e: Expr): void {
  switch (e.kind) {
    case "scalarSubquery":
    case "exists":
    case "inSubquery":
    case "quantifiedSubquery":
      throw engineError(
        "feature_not_supported",
        "cannot use subquery in column generation expression",
      );
    case "param":
      throw engineError("undefined_parameter", "there is no parameter $" + e.index.toString());
    case "funcCall":
      if (
        (e.over !== undefined && e.over !== null) ||
        (e.overName !== undefined && e.overName !== null)
      ) {
        throw engineError(
          "windowing_error",
          "window functions are not allowed in column generation expressions",
        );
      }
      if (isAggregateName(e.name)) {
        throw engineError(
          "grouping_error",
          "aggregate functions are not allowed in column generation expressions",
        );
      }
      if (isSetReturningName(e.name)) {
        throw engineError(
          "feature_not_supported",
          "set-returning functions are not allowed in column generation expressions",
        );
      }
      break;
    default:
      break;
  }
  for (const c of exprChildren(e)) rejectGeneratedStructure(c);
}

// exprChildren is the direct sub-expressions of e a row-level walk descends into (a subquery's body
// is not one: it is rejected before its children matter).
function exprChildren(e: Expr): Expr[] {
  switch (e.kind) {
    case "row":
      return e.fields;
    case "array":
      return e.elements;
    case "coalesce":
    case "greatestLeast":
    case "nullIf":
      return e.args;
    case "fieldAccess":
    case "fieldStar":
      return [e.base];
    case "subscript":
      return [e.base, ...astSubscriptExprs(e.subscripts)];
    case "cast":
    case "collate":
      return [e.inner];
    case "extract":
      return [e.source];
    case "unary":
    case "isNull":
    case "isJson":
    case "jsonCtor":
      return [e.operand];
    case "jsonExists":
    case "jsonValue":
    case "jsonQuery":
      return [e.ctx, e.path, ...e.passing.map((p) => p.expr)];
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return [e.lhs, e.rhs];
    case "in":
      return [e.lhs, ...e.list];
    case "between":
      return [e.lhs, e.lo, e.hi];
    case "case": {
      const v: Expr[] = [];
      if (e.operand !== null) v.push(e.operand);
      for (const w of e.whens) v.push(w.cond, w.result);
      if (e.els !== null) v.push(e.els);
      return v;
    }
    case "funcCall": {
      const v: Expr[] = [...e.args];
      if (e.filter !== undefined && e.filter !== null) v.push(e.filter);
      return v;
    }
    case "quantified":
      return [e.lhs, e.array];
    default: // column, qualifiedColumn, literal, typed literal, qualifiedStar, param, subqueries
      return [];
  }
}

// isSetReturningName reports the set-returning built-ins a scalar call position can name
// (functions.md §10). In a generation expression each is 0A000, PostgreSQL's rejection.
function isSetReturningName(name: string): boolean {
  switch (name) {
    case "generate_series":
    case "generate_subscripts":
    case "unnest":
    case "regexp_matches":
    case "regexp_split_to_table":
    case "string_to_table":
    case "json_array_elements":
    case "json_array_elements_text":
    case "jsonb_array_elements":
    case "jsonb_array_elements_text":
    case "json_each":
    case "json_each_text":
    case "jsonb_each":
    case "jsonb_each_text":
    case "json_object_keys":
    case "jsonb_object_keys":
    case "jsonb_path_query":
    case "json_populate_recordset":
    case "jsonb_populate_recordset":
    case "json_to_recordset":
    case "jsonb_to_recordset":
      return true;
    default:
      return false;
  }
}

// generatedRecomputeSet reports which generated columns an UPDATE recomputes (§3.2): exactly those
// whose expression references one of the assigned columns (assigned[i] marks column i a SET
// target). null when the table has no generated column the statement touches.
export function generatedRecomputeSet(
  table: Table,
  assigned: readonly boolean[],
): boolean[] | null {
  const need = table.columns.map(() => false);
  let any = false;
  table.columns.forEach((col, i) => {
    if (!col.generated) return;
    if (checkReferencedColumns(col.generated.expr, table.columns).some((r) => assigned[r])) {
      need[i] = true;
      any = true;
    }
  });
  return any ? need : null;
}

// generatedInsertError is the 428C9 for an INSERT supplying a non-DEFAULT value to a generated
// column.
export function generatedInsertError(column: string): EngineError {
  return engineError("generated_always", `cannot insert a non-DEFAULT value into column ${column}`)
    .withDetail(`Column "${column}" is a generated column.`)
    .noHint();
}

// generatedUpdateError is the 428C9 for an UPDATE assigning a non-DEFAULT value to a generated
// column.
export function generatedUpdateError(column: string): EngineError {
  return engineError("generated_always", `column ${column} can only be updated to DEFAULT`)
    .withDetail(`Column "${column}" is a generated column.`)
    .noHint();
}

// insertTargetedError is the 428C9 for an INSERT target column that only takes DEFAULT: a stored
// generated column (§3.1) or a GENERATED ALWAYS identity column (sequences.md §13.3).
export function insertTargetedError(col: Column): EngineError {
  return col.generated ? generatedInsertError(col.name) : generatedAlwaysInsert(col.name);
}

// checkFkGeneratedActions: a foreign key whose referencing columns include a generated column may
// not have an action that writes them (§2 "Foreign keys"): ON UPDATE CASCADE / SET NULL / SET
// DEFAULT and ON DELETE SET NULL / SET DEFAULT are 42601, PostgreSQL's rule.
export function checkFkGeneratedActions(
  table: Table,
  local: readonly number[],
  onDelete: FkAction,
  onUpdate: FkAction,
): void {
  if (!local.some((i) => table.columns[i]!.generated)) return;
  if (onUpdate === "cascade" || onUpdate === "setNull" || onUpdate === "setDefault") {
    throw engineError(
      "syntax_error",
      "invalid ON UPDATE action for foreign key constraint containing generated column",
    );
  }
  if (onDelete === "setNull" || onDelete === "setDefault") {
    throw engineError(
      "syntax_error",
      "invalid ON DELETE action for foreign key constraint containing generated column",
    );
  }
}

// generatedReaderOf is the first generated column (in column order) whose expression references
// column ci, or -1.
export function generatedReaderOf(table: Table, ci: number): number {
  return table.columns.findIndex(
    (c) => !!c.generated && checkReferencedColumns(c.generated.expr, table.columns).includes(ci),
  );
}

// generatedDependents is the names of the generated columns (other than ci itself) whose
// expression references column ci, in column order.
export function generatedDependents(table: Table, ci: number): string[] {
  const out: string[] = [];
  table.columns.forEach((c, j) => {
    if (
      j !== ci &&
      c.generated &&
      checkReferencedColumns(c.generated.expr, table.columns).includes(ci)
    ) {
      out.push(c.name);
    }
  });
  return out;
}
