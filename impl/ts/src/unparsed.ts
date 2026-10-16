// Stored definitions this binary cannot parse (spec/design/unparsable-definitions.md). The catalog
// load keeps a view, CHECK, expression DEFAULT, or expression/partial index whose stored text the
// parser rejects, marked unparsed with its text verbatim. Only a statement that needs the parsed form
// fails, with XX003, before it reads or writes a row.

import type { Expr } from "./ast.ts";
import type { IndexDef, Table } from "./catalog.ts";
import { EngineError, engineError } from "./errors.ts";
import { parseExpression } from "./parser.ts";

function unparsableError(detail: string): EngineError {
  return engineError(
    "unparsable_stored_definition",
    "stored definition does not parse in this version of jed: " + detail,
  );
}

export function unparsableViewError(name: string): EngineError {
  return unparsableError('view "' + name + '"');
}

export function unparsableCheckError(table: string, check: string): EngineError {
  return unparsableError('check constraint "' + check + '" of table "' + table + '"');
}

export function unparsableDefaultError(table: string, column: string): EngineError {
  return unparsableError('default of column "' + column + '" of table "' + table + '"');
}

export function unparsableIndexError(table: string, index: string): EngineError {
  return unparsableError('index "' + index + '" of table "' + table + '"');
}

// isUnparsableError reports whether e is XX003. View re-validation reports a dependent that fails to
// plan as the generic 0A000; an unparsed view reached while planning it passes through instead, so
// the error names the view that needs a newer jed.
export function isUnparsableError(e: unknown): boolean {
  return e instanceof EngineError && e.state === "unparsable_stored_definition";
}

// parseStoredExpression parses a stored expression's canonical text. Text this binary's parser
// rejects is not corruption (§2): it yields a placeholder and unparsed, and the object is kept.
export function parseStoredExpression(text: string): { expr: Expr; unparsed?: true } {
  try {
    return { expr: parseExpression(text) };
  } catch {
    return { expr: { kind: "literal", literal: { kind: "null" } }, unparsed: true };
  }
}

// indexUnparsed reports whether an expression key or the predicate of the index did not parse (§3.4).
export function indexUnparsed(def: IndexDef): boolean {
  return (
    def.keys.some((k) => k.kind === "expr" && k.unparsed === true) ||
    def.predicate?.unparsed === true
  );
}

// assertTableParsed throws XX003 for the first unparsed check (name order), expression default
// (column order, when defaults), or index (catalog order) of t. The ALTER TABLE actions that analyze
// or rewrite every stored expression of a table call it before they change anything (§3.2–§3.4); DROP
// COLUMN passes defaults = false, since a default cannot reference a column.
export function assertTableParsed(t: Table, defaults: boolean): void {
  for (const c of t.checks) {
    if (c.unparsed) throw unparsableCheckError(t.name, c.name);
  }
  if (defaults) {
    for (const col of t.columns) {
      if (col.defaultExpr?.unparsed) throw unparsableDefaultError(t.name, col.name);
    }
  }
  for (const ix of t.indexes) {
    if (indexUnparsed(ix)) throw unparsableIndexError(t.name, ix.name);
  }
}
