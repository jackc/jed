// Structured error codes (CLAUDE.md §5, §10). A SqlState's code is the canonical 5-char
// SQLSTATE from spec/errors/registry.toml. The SqlState union + the code mapping are
// generated (the codegen "middle path", CLAUDE.md §5 — see sqlstate.ts /
// spec/design/codegen.md); this file is the hand-written EngineError scaffolding that
// consumes them. Errors are thrown as EngineError (the TS idiom); the harness reads
// `.code()` to match `statement error <code>`.

import { type SqlState, sqlStateCode } from "./sqlstate.ts";

// Re-export so existing `./errors.ts` consumers (and lib.ts) keep their import paths.
export { type SqlState, sqlStateCode };

// EngineError is an engine error: a SQLSTATE plus an informational (never-matched)
// message. The message text embeds the code so it also matches as a plain regex under
// a stock sqllogictest runner (spec/design/conformance.md §2).
//
// Beyond code + message it carries optional structured diagnostic fields
// (spec/design/error-fields.md §3) modeled on pgx's pgconn.PgError — the constraint /
// table / column / data-type name a host would otherwise scrape from the message text.
// An `undefined` field means it does not apply to this error.
export class EngineError extends Error {
  state: SqlState;
  constraintName?: string;
  tableName?: string;
  columnName?: string;
  dataTypeName?: string;

  constructor(state: SqlState, message: string) {
    super(`${sqlStateCode(state)}: ${message}`);
    this.name = "EngineError";
    this.state = state;
  }

  // code returns the error's SQLSTATE string.
  code(): string {
    return sqlStateCode(this.state);
  }

  // --- structured-field builders (spec/design/error-fields.md §5) --------------------
  // Chainable setters. engineError() leaves every field undefined, so existing call sites
  // are unaffected; only the raise sites that know an identifier opt in.
  withConstraint(name: string): this {
    this.constraintName = name;
    return this;
  }
  withTable(name: string): this {
    this.tableName = name;
    return this;
  }
  withColumn(name: string): this {
    this.columnName = name;
    return this;
  }
  withDataType(name: string): this {
    this.dataTypeName = name;
    return this;
  }
}

// engineError builds an EngineError (mirrors Go's NewError / Rust's EngineError::new).
export function engineError(state: SqlState, message: string): EngineError {
  return new EngineError(state, message);
}

// stampTable annotates a column-store failure (23502 / 22003 / 22001) with the target
// relation: the relation name is in scope at the DML boundary, not inside the coercion
// (spec/design/error-fields.md §4). A no-op for a non-EngineError. Rethrow the result.
export function stampTable(err: unknown, table: string): unknown {
  if (err instanceof EngineError) err.tableName = table;
  return err;
}

// pkeyName is the derived <table>_pkey constraint name PostgreSQL auto-assigns a primary key
// (jed persists no such relation — constraints.md §5.4).
export function pkeyName(table: string): string {
  return table.toLowerCase() + "_pkey";
}

// --- typed integrity-violation constructors (spec/design/error-fields.md §5) ----------
// One source per message template (mirrors spec/errors/registry.toml), so the prose and the
// structured field can never drift apart. Message text is byte-identical to the prior inline
// concatenations — the conformance corpus matches on it.

// uniqueViolation — 23505 for a duplicate key under `constraint` (a unique index's name, or
// the derived <table>_pkey) on `table`.
export function uniqueViolation(table: string, constraint: string): EngineError {
  return engineError(
    "unique_violation",
    "duplicate key value violates unique constraint: " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// checkViolation — 23514, a row fails CHECK `constraint` on `table`.
export function checkViolation(table: string, constraint: string): EngineError {
  return engineError(
    "check_violation",
    "new row for relation " + table + " violates check constraint " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// fkViolationInsert — 23503 child side, an INSERT/UPDATE on `table` references a parent key
// absent under FK `constraint`.
export function fkViolationInsert(table: string, constraint: string): EngineError {
  return engineError(
    "foreign_key_violation",
    "insert or update on table " + table + " violates foreign key constraint " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// fkViolationDelete — 23503 parent side, a DELETE/UPDATE on `parent` strands a child of
// `child` still referencing it under FK `constraint`.
export function fkViolationDelete(parent: string, constraint: string, child: string): EngineError {
  return engineError(
    "foreign_key_violation",
    "update or delete on table " +
      parent +
      " violates foreign key constraint " +
      constraint +
      " on table " +
      child,
  )
    .withTable(parent)
    .withConstraint(constraint);
}

// exclusionViolation — 23P01, a row conflicts with an existing one under EXCLUDE `constraint`
// (the backing GiST index's name) on `table`.
export function exclusionViolation(table: string, constraint: string): EngineError {
  return engineError(
    "exclusion_violation",
    "conflicting key value violates exclusion constraint: " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// notNullViolation — 23502, a NULL into NOT NULL `column`. The table is stamped at the DML
// boundary (stampTable), where the relation name is in scope.
export function notNullViolation(column: string): EngineError {
  return engineError(
    "not_null_violation",
    "null value in column " + column + " violates not-null constraint",
  ).withColumn(column);
}
