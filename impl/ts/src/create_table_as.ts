// `CREATE TABLE … AS query [WITH [NO] DATA]` (spec/design/create-table-as.md): a table whose columns
// are the query's output columns and whose rows are the query's rows. The query is planned once to
// derive the columns — each output's name (or the column list's), type, typmod, and collation — then
// the table is created through CREATE TABLE's shared registration, and `WITH DATA` fills it by running
// `INSERT INTO <table> <query>` in the same statement, so the rows, the cost, and the errors are
// exactly that INSERT's. The Engine method that runs the statement (executeCreateTableAs) lives in
// executor.ts; this file holds the column derivation. Mirrors impl/rust/src/executor/create_table_as.rs
// and impl/go create_table_as.go.

import type { CreateTable, CreateTableAs } from "./ast.ts";
import type { Column } from "./catalog.ts";
import { engineError } from "./errors.ts";
import {
  type QueryPlan,
  type ResolvedType,
  queryPlanColumnDerivs,
  queryPlanColumnTypmods,
} from "./executor.ts";
import { TYPMOD_NONE } from "./resolve.ts";
import { rtCollatable, rtName, typeFromResolved } from "./scope.ts";
import { type Type, typeCanonicalName, typeIsCollatable } from "./types.ts";

// ctasColumns derives the new table's columns from the planned query (create-table-as.md §2): the
// column list's names over the query's output names, the output types (an untyped NULL is text), the
// typmods the outputs keep, and each text column's collation. A table column has no constraint,
// default, or key. defaultCollation is the database's default collation (null for C), which a text
// column that derives no collation takes. The checks run in PostgreSQL's order: per column an
// underivable collation (42P22), then too many column names (42601), ON COMMIT on a table that is not
// temporary (42P16), a repeated column name (42701), and a column type no table can have.
export function ctasColumns(
  ct: CreateTable,
  cta: CreateTableAs,
  plan: QueryPlan,
  targetTemp: boolean,
  defaultCollation: string | null,
): Column[] {
  const types = plan.columnTypes;
  const derivs = queryPlanColumnDerivs(plan);
  const typmods = queryPlanColumnTypmods(plan);
  const renames = cta.columnNames ?? [];
  const names = plan.columnNames.map((n, i) => renames[i] ?? n);
  // A text column whose collation could not be derived (two different implicit collations met in a
  // UNION ALL, VALUES, or expression) cannot be created: PostgreSQL would otherwise give it a default
  // it never chose.
  types.forEach((t, i) => {
    const d = derivs[i]!;
    if (rtCollatable(t) && (d.kind === "indeterminate" || d.kind === "conflicted")) {
      throw engineError(
        "indeterminate_collation",
        `no collation was derived for column "${names[i]!}" with collatable type ${rtName(t)}`,
      );
    }
  });
  if (renames.length > types.length) {
    throw engineError("syntax_error", "too many column names were specified");
  }
  if (ct.onCommit !== undefined && !targetTemp) {
    throw engineError("invalid_table_definition", "ON COMMIT can only be used on temporary tables");
  }
  names.forEach((name, i) => {
    if (names.slice(0, i).includes(name)) {
      throw engineError("duplicate_column", "duplicate column name: " + name);
    }
  });
  return types.map((rt, i) => {
    const ty = ctasColumnType(names[i]!, rt);
    const typmod = ty.kind === "scalar" ? typmods[i]! : TYPMOD_NONE;
    let collation: string | null = null;
    if (typeIsCollatable(ty)) {
      const d = derivs[i]!;
      if (d.kind === "implicit" || d.kind === "explicit") {
        collation = d.name !== "C" ? d.name : null;
      } else {
        // A column that derives no collation (a literal, an untyped NULL) takes the database's
        // default, as a declared column without COLLATE does.
        collation = defaultCollation;
      }
    }
    return {
      name: names[i]!,
      type: ty,
      decimal: typmod.decimal,
      varcharLen: typmod.varcharLen,
      primaryKey: false,
      notNull: false,
      default: null,
      defaultExpr: null,
      identity: null,
      collation,
    };
  });
}

// rejectPseudoType rejects a persisted relation column (a CREATE TABLE … AS table's or a view's)
// whose type is an anonymous ROW(…) or an array of one: PostgreSQL's pseudo-type record / record[],
// which only a query's own output may carry (spec/design/composite.md §1) — 42P16.
export function rejectPseudoType(name: string, rt: ResolvedType): void {
  const anonymous = (t: ResolvedType): boolean => t.kind === "composite" && t.name === null;
  let pseudo: string | null = null;
  if (anonymous(rt)) pseudo = "record";
  else if (rt.kind === "array" && anonymous(rt.elem)) pseudo = "record[]";
  if (pseudo !== null) {
    throw engineError("invalid_table_definition", `column "${name}" has pseudo-type ${pseudo}`);
  }
}

// ctasColumnType is the table-column type of a query output column of resolved type rt
// (create-table-as.md §2): an untyped NULL is text, as in PostgreSQL. An anonymous ROW(…) (or an array
// of one) has no storable type — PostgreSQL's pseudo-type record, 42P16. The types a declared column
// cannot have in jed either are the same 0A000: a jsonpath column, and an array whose element is
// itself an array or a range.
function ctasColumnType(name: string, rt: ResolvedType): Type {
  rejectPseudoType(name, rt);
  const ty = typeFromResolved(rt);
  let supported: boolean;
  switch (ty.kind) {
    case "scalar":
      supported = ty.scalar !== "jsonpath";
      break;
    case "array":
      supported = ty.elem.kind === "scalar" || ty.elem.kind === "composite";
      break;
    case "record":
      // Unreachable: rejectPseudoType rejects a record (and a record[]) first.
      supported = false;
      break;
    default:
      supported = true;
  }
  if (!supported) {
    throw engineError(
      "feature_not_supported",
      `a ${typeCanonicalName(ty)} column is not supported yet`,
    );
  }
  return ty;
}
